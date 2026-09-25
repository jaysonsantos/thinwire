//! Worker bookkeeping: which TDLib workers are still closing.
//!
//! A new client must not open the TDLib database while an old client still
//! holds its lock. Each worker owns a done flag. The runtime keeps the flag
//! of the current worker and the flags of workers that are closing. No TDLib
//! types here, so default builds test it.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::adapter::{
    AdapterEvent, AdapterStatus, EventTx, LoginEpoch, ProtocolId, TelegramAuthError,
    TelegramAuthPhase, TelegramCodeVia, emit_telegram_data_reset,
};

/// One client's identity for login events: its login epoch, and a flag the
/// runtime sets when it asks the client to close (Cancel, Try again,
/// shutdown), before the worker reads its `Close` command.
#[derive(Debug, Clone, Default)]
pub(super) struct ClosingFlag {
    closing: Arc<AtomicBool>,
    epoch: u64,
    /// The host's login epoch. The UI thread bumps it at Cancel, before the
    /// worker reads `Close`, so the worker can stop at once.
    shared: LoginEpoch,
}

impl ClosingFlag {
    /// A new client started under the host's current login epoch.
    pub(super) fn new(shared: LoginEpoch) -> Self {
        Self {
            closing: Arc::new(AtomicBool::new(false)),
            epoch: shared.load(Ordering::SeqCst),
            shared,
        }
    }

    /// `true` while this client may still act on login results: it is not
    /// closing, and Cancel has not moved the host's epoch past it.
    #[must_use]
    pub(super) fn current(&self) -> bool {
        !self.is_set() && self.shared.load(Ordering::SeqCst) == self.epoch
    }

    pub(super) fn mark(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub(super) fn is_set(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }
}

/// Login events of one client. A client that is closing sends none of them,
/// so a late `NeedPhone` or `Ready` cannot reopen a cancelled form or link a
/// dead inbox (issue #42). Status lines and `Stopped` are not login events.
pub(super) struct LoginEvents<'a> {
    events: &'a EventTx,
    closing: &'a ClosingFlag,
}

impl<'a> LoginEvents<'a> {
    pub(super) const fn new(events: &'a EventTx, closing: &'a ClosingFlag) -> Self {
        Self { events, closing }
    }

    /// `false` once the client is closing or Cancel moved the epoch. The
    /// worker checks it after each TDLib call returns (PR #49 review).
    #[must_use]
    pub(super) fn open(&self) -> bool {
        self.closing.current()
    }

    /// Send `event` stamped with this client's epoch, while it is not closing.
    /// The host drops it later if Cancel bumped the epoch meanwhile.
    fn send(&self, event: AdapterEvent) {
        if self.open() {
            let _ = self.events.send(AdapterEvent::Login {
                epoch: self.closing.epoch,
                event: Box::new(event),
            });
        }
    }

    pub(super) fn phase(&self, phase: TelegramAuthPhase) {
        self.send(AdapterEvent::TelegramAuth { phase });
    }

    pub(super) fn rejected(&self, error: TelegramAuthError) {
        self.send(AdapterEvent::TelegramAuthRejected { error });
    }

    pub(super) fn code_sent(&self, via: TelegramCodeVia) {
        self.send(AdapterEvent::TelegramCodeSent { via });
    }

    /// A status line of a login step, for example a refused code. It carries
    /// the epoch too, so a late RPC result never shows on a newer login (#54).
    pub(super) fn status(&self, status: AdapterStatus, detail: &str) {
        self.send(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status,
            detail: detail.into(),
        });
    }

    /// Not a login step: a folder already moved on disk. It goes out even
    /// when the client is closing, unstamped, so the next phone step can name
    /// the kept folder (PR #49 review).
    pub(super) fn data_reset(&self, moved_to: &str) {
        emit_telegram_data_reset(self.events, moved_to);
    }
}

/// How a worker ends its TDLib client on `Close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseKind {
    /// Keep the session (app shutdown, or not signed in).
    Close,
    /// Cancel reached a client that became Ready from a login in this worker
    /// (phone, code, or password step), but the UI dropped that Ready: roll
    /// back. Clear the session marker and log out.
    LogOut,
}

/// `new_login` is true only when this worker asked for a login step before
/// Ready. A resumed saved session never asks, so a Cancel that races its
/// Ready never logs it out (ux F8: Cancel does not end a live session).
#[must_use]
pub(super) const fn close_kind(authorized: bool, new_login: bool, cancel: bool) -> CloseKind {
    if authorized && new_login && cancel {
        CloseKind::LogOut
    } else {
        CloseKind::Close
    }
}

/// What a worker does with a Ready that its closing flag or a moved epoch
/// told it to skip. TDLib is signed in even though the UI never links it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LateReady {
    /// The worker has not read `Close` yet. Remember the Ready; `close_kind`
    /// then decides on `Close`.
    Wait,
    /// Cancel already closed this new login: log out now (PR #49 review).
    LogOut,
    /// Shutdown, or a resumed session: keep it (ux F8).
    Keep,
}

/// `close_cancel` is `None` before the worker reads `Close`, else the
/// `cancel` value of that `Close`.
#[must_use]
pub(super) const fn late_ready(new_login: bool, close_cancel: Option<bool>) -> LateReady {
    match close_cancel {
        None => LateReady::Wait,
        Some(true) if new_login => LateReady::LogOut,
        Some(_) => LateReady::Keep,
    }
}

/// Set by a worker when it has exited and released its client.
pub(super) type DoneFlag = Arc<AtomicBool>;

#[derive(Debug, Default)]
pub(super) struct WorkerSlots {
    current: Option<DoneFlag>,
    retiring: Vec<DoneFlag>,
    /// The app is closing. No new worker may start.
    shut: bool,
}

impl WorkerSlots {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The current worker is closing. The next worker must wait for it.
    pub(super) fn retire_current(&mut self) {
        if let Some(current) = self.current.take() {
            self.retiring.push(current);
        }
    }

    /// The app is closing: retire the current worker and refuse new ones.
    pub(super) fn shut_down(&mut self) {
        self.shut = true;
        self.retire_current();
    }

    #[must_use]
    pub(super) fn is_shut(&self) -> bool {
        self.shut
    }

    /// Start a new current worker. Returns its done flag and the flags it
    /// must wait for before it opens a client. `None` after [`Self::shut_down`].
    pub(super) fn start(&mut self) -> Option<(DoneFlag, Vec<DoneFlag>)> {
        if self.shut {
            return None;
        }
        self.retire_current();
        self.retiring.retain(|flag| !is_done(flag));
        let done = Arc::new(AtomicBool::new(false));
        self.current = Some(Arc::clone(&done));
        Some((done, self.retiring.clone()))
    }

    /// Every flag known now, current and retiring. Shutdown waits for these.
    pub(super) fn all(&self) -> Vec<DoneFlag> {
        self.retiring.iter().chain(&self.current).cloned().collect()
    }
}

#[must_use]
pub(super) fn is_done(flag: &DoneFlag) -> bool {
    flag.load(Ordering::SeqCst)
}

#[must_use]
pub(super) fn all_done(flags: &[DoneFlag]) -> bool {
    flags.iter().all(is_done)
}

pub(super) fn mark_done(flag: &DoneFlag) {
    flag.store(true, Ordering::SeqCst);
}

/// Poll `done` until it is true or `limit` passes. `true` when it finished in
/// time. Shutdown uses this, so a stuck worker cannot hold the exit forever.
pub(super) async fn wait_until(done: impl Fn() -> bool, limit: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_worker_waits_for_the_worker_that_is_closing() {
        let mut slots = WorkerSlots::new();
        let (first, wait) = slots.start().expect("open");
        assert!(wait.is_empty(), "nothing to wait for at first");
        // Cancel → Add account: the first worker is closing, a second starts.
        slots.retire_current();
        let (second, wait) = slots.start().expect("open");
        assert_eq!(wait.len(), 1);
        assert!(
            !all_done(&wait),
            "the first client still holds the database"
        );
        mark_done(&first);
        assert!(all_done(&wait));
        assert_eq!(slots.all().len(), 2);
        mark_done(&second);
        assert!(all_done(&slots.all()));
    }

    #[test]
    fn finished_workers_drop_out_of_the_wait_list() {
        let mut slots = WorkerSlots::new();
        let (first, _) = slots.start().expect("open");
        mark_done(&first);
        let (_second, wait) = slots.start().expect("open");
        assert!(wait.is_empty(), "a done worker is not waited for");
        slots.retire_current();
        assert_eq!(slots.all().len(), 1);
    }

    #[test]
    fn a_closing_client_sends_no_login_events() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared: LoginEpoch = Arc::new(std::sync::atomic::AtomicU64::new(7));
        let closing = ClosingFlag::new(Arc::clone(&shared));
        // The runtime keeps a clone; Cancel marks it before `Close` is read.
        let runtime_side = closing.clone();
        let login = LoginEvents::new(&tx, &closing);
        login.phase(TelegramAuthPhase::NeedCode);
        assert_eq!(
            rx.try_recv().expect("open client"),
            AdapterEvent::Login {
                epoch: 7,
                event: Box::new(AdapterEvent::TelegramAuth {
                    phase: TelegramAuthPhase::NeedCode
                }),
            },
            "stamped with the client's epoch"
        );

        runtime_side.mark();
        // A fake closing client reports late login updates: none go out.
        login.phase(TelegramAuthPhase::NeedPhone);
        login.phase(TelegramAuthPhase::Ready);
        login.phase(TelegramAuthPhase::Failed);
        login.rejected(TelegramAuthError::CodeInvalid);
        login.code_sent(TelegramCodeVia::Sms);
        assert!(
            rx.try_recv().is_err(),
            "no login event after Cancel (issue #42)"
        );
        assert!(!login.open());

        // The data-reset notice is file state, not a login step: it still goes out.
        login.data_reset("tdlib.stale-1");
        assert_eq!(
            rx.try_recv().expect("kept notice"),
            AdapterEvent::TelegramDataReset {
                moved_to: "tdlib.stale-1".into()
            }
        );

        // A new client has its own flag: a clean login state.
        let fresh = ClosingFlag::new(Arc::new(std::sync::atomic::AtomicU64::new(8)));
        LoginEvents::new(&tx, &fresh).phase(TelegramAuthPhase::NeedPhone);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn cancel_stops_a_client_before_its_ready_side_effects() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared: LoginEpoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let closing = ClosingFlag::new(Arc::clone(&shared));
        let login = LoginEvents::new(&tx, &closing);
        assert!(login.open());
        // Cancel: the UI thread bumps the epoch. The worker has not read
        // `Close` yet, so the closing flag is still clear.
        shared.fetch_add(1, Ordering::SeqCst);
        assert!(!closing.is_set());
        assert!(!login.open(), "the worker skips Ready side effects at once");
        login.phase(TelegramAuthPhase::Ready);
        login.phase(TelegramAuthPhase::Failed);
        assert!(
            rx.try_recv().is_err(),
            "a result after Cancel is dropped at the source"
        );
    }

    #[test]
    fn a_late_rpc_failure_of_a_cancelled_login_never_reaches_the_next_login() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared: LoginEpoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Login A sends its phone. The RPC is still pending.
        let closing_a = ClosingFlag::new(Arc::clone(&shared));
        let login_a = LoginEvents::new(&tx, &closing_a);
        login_a.status(AdapterStatus::Connecting, "step sent");
        let Some(AdapterEvent::Login { epoch: 0, event }) = rx.try_recv().ok() else {
            panic!("a login status carries its epoch");
        };
        assert!(matches!(*event, AdapterEvent::Status { .. }));

        // Cancel, then login B starts on epoch 1.
        shared.fetch_add(1, Ordering::SeqCst);
        closing_a.mark();
        let closing_b = ClosingFlag::new(Arc::clone(&shared));
        let login_b = LoginEvents::new(&tx, &closing_b);

        // Login A's RPC fails now: no rejection, no phase, no error status.
        login_a.rejected(TelegramAuthError::PhoneInvalid);
        login_a.phase(TelegramAuthPhase::Failed);
        login_a.status(AdapterStatus::Error, "Telegram rejected the phone number.");
        assert!(rx.try_recv().is_err(), "login B sees nothing from login A");

        login_b.status(AdapterStatus::Error, "Telegram rejected the login code.");
        assert!(matches!(
            rx.try_recv(),
            Ok(AdapterEvent::Login { epoch: 1, .. })
        ));
        // Every status of a login step goes through `LoginEvents::status`.
        let src = include_str!("tdlib.rs");
        let step = &src[src.find("async fn apply_step(").expect("apply_step")..];
        let step = &step[..step.find("\n}\n").expect("end")];
        assert!(!step.contains("emit_status("), "no unstamped step status");
        assert!(
            step.matches("login.status(").count() >= 5,
            "each error status"
        );
    }

    #[test]
    fn only_a_cancelled_signed_in_client_is_rolled_back() {
        assert_eq!(close_kind(true, true, true), CloseKind::LogOut);
        assert_eq!(
            close_kind(true, true, false),
            CloseKind::Close,
            "shutdown keeps the session"
        );
        assert_eq!(
            close_kind(false, true, true),
            CloseKind::Close,
            "nothing to roll back"
        );
        assert_eq!(close_kind(false, false, false), CloseKind::Close);
    }

    #[test]
    fn cancel_then_a_late_ready_logs_out_only_a_new_login() {
        // Cancel marks the flag first, so Ready is skipped before the worker
        // reads Close. The skipped Ready counts as signed in for close_kind.
        assert_eq!(late_ready(true, None), LateReady::Wait);
        assert_eq!(close_kind(true, true, true), CloseKind::LogOut);
        // The worker read Close first, then Ready came.
        assert_eq!(late_ready(true, Some(true)), LateReady::LogOut);
        // A resumed session: no logout on either path.
        assert_eq!(late_ready(false, None), LateReady::Wait);
        assert_eq!(close_kind(true, false, true), CloseKind::Close);
        assert_eq!(late_ready(false, Some(true)), LateReady::Keep);
        // Shutdown keeps a new login too.
        assert_eq!(late_ready(true, Some(false)), LateReady::Keep);
        assert_eq!(close_kind(true, true, false), CloseKind::Close);
    }

    #[test]
    fn cancel_never_logs_out_a_resumed_session() {
        // A saved session goes to Ready with no login step. A Cancel that
        // races that Ready closes the client and keeps the session (ux F8).
        assert_eq!(close_kind(true, false, true), CloseKind::Close);
        assert_eq!(close_kind(true, false, false), CloseKind::Close);
    }

    #[tokio::test]
    async fn wait_until_returns_when_done_and_gives_up_at_the_limit() {
        use std::sync::atomic::AtomicUsize;
        let polls = AtomicUsize::new(0);
        let finished = wait_until(
            || polls.fetch_add(1, Ordering::SeqCst) >= 2,
            Duration::from_secs(1),
            Duration::from_millis(1),
        )
        .await;
        assert!(finished);
        assert_eq!(polls.load(Ordering::SeqCst), 3);

        let started = Instant::now();
        let stuck = wait_until(
            || false,
            Duration::from_millis(30),
            Duration::from_millis(5),
        )
        .await;
        assert!(!stuck, "a fake that never finishes hits the limit");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn no_worker_starts_after_shutdown() {
        let mut slots = WorkerSlots::new();
        let (current, _) = slots.start().expect("open");
        slots.shut_down();
        assert!(slots.is_shut());
        assert!(
            slots.start().is_none(),
            "a click during close starts nothing"
        );
        assert_eq!(
            slots.all().len(),
            1,
            "shutdown still waits for the old worker"
        );
        mark_done(&current);
        assert!(all_done(&slots.all()));
    }
}
