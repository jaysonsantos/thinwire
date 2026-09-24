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
    EventTx, TelegramAuthError, TelegramAuthPhase, TelegramCodeVia, emit_telegram_auth,
    emit_telegram_auth_rejected, emit_telegram_code_sent, emit_telegram_data_reset,
};

/// Set by the runtime when it asks a worker's client to close (Cancel, Try
/// again, shutdown). It is set before the worker reads its `Close` command.
#[derive(Debug, Clone, Default)]
pub(super) struct ClosingFlag(Arc<AtomicBool>);

impl ClosingFlag {
    pub(super) fn mark(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub(super) fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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

    /// `false` once the client is closing.
    #[must_use]
    pub(super) fn open(&self) -> bool {
        !self.closing.is_set()
    }

    pub(super) fn phase(&self, phase: TelegramAuthPhase) {
        if self.open() {
            emit_telegram_auth(self.events, phase);
        }
    }

    pub(super) fn rejected(&self, error: TelegramAuthError) {
        if self.open() {
            emit_telegram_auth_rejected(self.events, error);
        }
    }

    pub(super) fn code_sent(&self, via: TelegramCodeVia) {
        if self.open() {
            emit_telegram_code_sent(self.events, via);
        }
    }

    pub(super) fn data_reset(&self, moved_to: &str) {
        if self.open() {
            emit_telegram_data_reset(self.events, moved_to);
        }
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
        use crate::adapter::AdapterEvent;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let closing = ClosingFlag::default();
        // The runtime keeps a clone; Cancel marks it before `Close` is read.
        let runtime_side = closing.clone();
        let login = LoginEvents::new(&tx, &closing);
        login.phase(TelegramAuthPhase::NeedCode);
        assert_eq!(
            rx.try_recv().expect("open client"),
            AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedCode
            }
        );

        runtime_side.mark();
        // A fake closing client reports late login updates: none reach the UI.
        login.phase(TelegramAuthPhase::NeedPhone);
        login.phase(TelegramAuthPhase::Ready);
        login.phase(TelegramAuthPhase::Failed);
        login.rejected(TelegramAuthError::CodeInvalid);
        login.code_sent(TelegramCodeVia::Sms);
        login.data_reset("tdlib.stale-1");
        assert!(
            rx.try_recv().is_err(),
            "no login event after Cancel (issue #42)"
        );
        assert!(!login.open());

        // A new client has its own flag: a clean login state.
        let fresh = ClosingFlag::default();
        LoginEvents::new(&tx, &fresh).phase(TelegramAuthPhase::NeedPhone);
        assert!(rx.try_recv().is_ok());
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
