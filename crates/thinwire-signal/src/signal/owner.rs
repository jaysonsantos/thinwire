// SPDX-License-Identifier: AGPL-3.0-only
//! One task owns a Signal link attempt.
//!
//! Begin, Resume, RetryKey, Cancel, Shutdown, and worker results are messages.
//! The task handles them one at a time. A new Begin runs only after the
//! previous cleanup finished, so a late shutdown cannot clear the next attempt.

use std::sync::Arc;

use thinwire_protocol::{
    AccountState, AdapterStatus, EventTx, ProtocolId, emit_account, emit_status, emit_stopped,
};
use tokio::sync::{mpsc, oneshot};

use super::live::Session;
use super::{SHUTDOWN_LIMIT, SignalWorker, finish_worker};

enum Msg {
    Begin {
        pairing: u64,
        events: EventTx,
    },
    /// Open a registration that is already on disk. No new QR link.
    Resume {
        events: EventTx,
    },
    Cancel {
        events: EventTx,
    },
    /// The user unlinks the account: ask Signal to remove this device, then
    /// delete the live session.
    Unlink {
        events: EventTx,
    },
    /// Read the session key again. This is not a new link.
    RetryKey {
        events: EventTx,
    },
    Shutdown {
        events: EventTx,
        done: oneshot::Sender<()>,
    },
    /// Test double: store a worker the owner did not start.
    #[cfg(test)]
    Attach {
        worker: SignalWorker,
    },
    #[cfg(test)]
    Flush {
        done: oneshot::Sender<()>,
    },
}

type StartFn = Box<dyn Fn(&Arc<Session>, &EventTx) -> Option<SignalWorker> + Send>;

/// Deletes the live session files. `false` when a file is still there. It
/// blocks, so the owner runs it off the async threads.
type DeleteFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Longest wait of an unlink for the answer of the worker. It is longer
/// than the wait of the worker for Signal. With no answer the unlink
/// continues: this computer forgets the device in each case.
const UNLINK_WAIT: std::time::Duration = std::time::Duration::from_secs(7);

/// Adapter side. Every method only sends a message.
pub(super) struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Handle {
    pub(super) fn spawn(session: Arc<Session>, start: StartFn) -> Self {
        Self::spawn_with_delete(session, start, Arc::new(super::live::delete_live_session))
    }

    /// [`Self::spawn`] with another delete of the live session files.
    pub(super) fn spawn_with_delete(
        session: Arc<Session>,
        start: StartFn,
        delete: DeleteFn,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let owner = Owner {
            session,
            start,
            delete,
            worker: None,
            generation: 0,
        };
        tokio::spawn(owner.run(rx));
        Self { tx }
    }

    pub(super) fn begin(&self, pairing: u64, events: &EventTx) {
        let _ = self.tx.send(Msg::Begin {
            pairing,
            events: events.clone(),
        });
    }

    /// Load a linked device after a remembered notice. No new provisioning.
    pub(super) fn resume(&self, events: &EventTx) {
        let _ = self.tx.send(Msg::Resume {
            events: events.clone(),
        });
    }

    pub(super) fn cancel(&self, events: &EventTx) {
        let _ = self.tx.send(Msg::Cancel {
            events: events.clone(),
        });
    }

    /// The user unlinks the account. The owner asks Signal to remove this
    /// device, deletes the live session, and sends `Unlinked`.
    pub(super) fn unlink(&self, events: &EventTx) {
        let _ = self.tx.send(Msg::Unlink {
            events: events.clone(),
        });
    }

    /// Read the session key again. Does not assign a new pairing id.
    pub(super) fn retry_key(&self, events: &EventTx) {
        let _ = self.tx.send(Msg::RetryKey {
            events: events.clone(),
        });
    }

    pub(super) fn shutdown(&self, events: &EventTx) {
        let (done, _wait) = oneshot::channel();
        let _ = self.tx.send(Msg::Shutdown {
            events: events.clone(),
            done,
        });
    }

    #[cfg(test)]
    pub(super) fn attach(&self, worker: SignalWorker) {
        let _ = self.tx.send(Msg::Attach { worker });
    }

    #[cfg(test)]
    pub(super) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Flush { done }).is_ok() {
            let _ = wait.await;
        }
    }
}

struct Owner {
    session: Arc<Session>,
    start: StartFn,
    delete: DeleteFn,
    worker: Option<SignalWorker>,
    generation: u64,
}

impl Owner {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Begin { pairing, events } => self.begin(pairing, &events).await,
                Msg::Resume { events } => self.resume(&events).await,
                Msg::RetryKey { events } => self.retry_key(&events).await,
                Msg::Cancel { events } => self.cancel(&events).await,
                Msg::Unlink { events } => self.unlink(&events).await,
                Msg::Shutdown { events, done } => {
                    self.shutdown(&events).await;
                    let _ = done.send(());
                }
                #[cfg(test)]
                Msg::Attach { worker } => {
                    self.worker = Some(worker);
                }
                #[cfg(test)]
                Msg::Flush { done } => {
                    let _ = done.send(());
                }
            }
        }
    }

    async fn begin(&mut self, pairing: u64, events: &EventTx) {
        if self.worker.is_some() {
            self.cleanup().await;
        }
        // A link the user starts can provision. A resume must not leak into it.
        self.session.set_resume_only(false);
        self.session.set_pairing(pairing);
        self.launch(events, true);
    }

    /// Open again with the pairing id that is already set.
    /// This does not start a new link, and a failed start does not end the account.
    /// The new open reads the key. A wrong key tries the move-aside again.
    async fn retry_key(&mut self, events: &EventTx) {
        if self.worker.is_some() {
            self.cleanup().await;
        }
        self.launch(events, false);
    }

    fn launch(&mut self, events: &EventTx, end_account_on_failure: bool) {
        // Connecting goes out before the worker thread runs, so the row
        // shows it while the store migration blocks.
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Connecting,
            "Signal linking was queued on the worker.",
        );
        self.worker = (self.start)(&self.session, events);
        self.generation = self.session.generation();
        if self.worker.is_none() {
            self.session.shutdown();
            emit_status(
                events,
                ProtocolId::Signal,
                AdapterStatus::Error,
                "Signal worker could not start. No session was opened.",
            );
            if end_account_on_failure {
                emit_account(events, ProtocolId::Signal, AccountState::Unlinked);
            }
        }
    }

    /// Load the registration when one exists. A missing store stays idle:
    /// no QR, and no link-failure status.
    async fn resume(&mut self, events: &EventTx) {
        if self.worker.is_some() {
            return;
        }
        self.session.set_resume_only(true);
        self.worker = (self.start)(&self.session, events);
        self.generation = self.session.generation();
        if self.worker.is_none() {
            self.session.shutdown();
        }
    }

    async fn cancel(&mut self, events: &EventTx) {
        self.cleanup().await;
        emit_account(events, ProtocolId::Signal, AccountState::Unlinked);
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            "Signal linking cancelled. No session is running.",
        );
    }

    /// The user unlinks the account. While the receive stream is open, the
    /// worker asks Signal to remove this device. This is best effort: with
    /// no connection, only this computer forgets the device, and the phone
    /// still lists it. Then the worker ends and the live session is
    /// deleted. The previous accounts under `retained/` stay.
    async fn unlink(&mut self, events: &EventTx) {
        if self.worker.is_some() {
            let (done, answer) = oneshot::channel();
            let confirmed = self.session.request_unlink(done)
                && matches!(
                    tokio::time::timeout(UNLINK_WAIT, answer).await,
                    Ok(Ok(true))
                );
            if !confirmed {
                tracing::warn!("signal did not confirm the unlink of this device");
            }
        }
        // The worker has the store open. It ends before the delete.
        self.cleanup().await;
        let delete = Arc::clone(&self.delete);
        let deleted = tokio::task::spawn_blocking(move || delete())
            .await
            .unwrap_or(false);
        emit_account(events, ProtocolId::Signal, AccountState::Unlinked);
        if deleted {
            emit_status(
                events,
                ProtocolId::Signal,
                AdapterStatus::Stubbed,
                super::live::UNLINKED,
            );
        } else {
            emit_status(
                events,
                ProtocolId::Signal,
                AdapterStatus::Error,
                super::live::UNLINK_FILES_LEFT,
            );
        }
    }

    async fn shutdown(&mut self, events: &EventTx) {
        self.cleanup().await;
        emit_account(events, ProtocolId::Signal, AccountState::Unlinked);
        emit_stopped(events, ProtocolId::Signal);
    }

    /// Cancel the current token, join the worker, then clear the session.
    /// The next message, including a new Begin, runs only after this returns.
    async fn cleanup(&mut self) {
        self.generation = self.session.cancel_attempt();
        if let Some(worker) = self.worker.take() {
            let _ = tokio::task::spawn_blocking(move || {
                finish_worker(worker, SHUTDOWN_LIMIT);
            })
            .await;
        }
        self.session.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use thinwire_protocol::{AccountState, AdapterEvent, AdapterStatus, ProtocolId};

    use super::*;

    fn stuck_worker(release: std::sync::mpsc::Receiver<()>) -> SignalWorker {
        let thread = std::thread::spawn(move || {
            let _ = release.recv();
        });
        let abort = tokio::spawn(async {}).abort_handle();
        SignalWorker { thread, abort }
    }

    #[tokio::test]
    async fn a_relink_waits_until_cancel_cleanup_finishes() {
        let session = Arc::new(Session::new());
        let starts = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(Some(release_rx)));
        let starts_for_start = Arc::clone(&starts);
        let handle = Handle::spawn(Arc::clone(&session), {
            let release_rx = Arc::clone(&release_rx);
            Box::new(move |session, _events| {
                let (_token, _wake) = session.begin_attempt();
                session.mark_active();
                let n = starts_for_start.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let rx = release_rx.lock().expect("release").take().expect("once");
                    Some(stuck_worker(rx))
                } else {
                    let thread = std::thread::spawn(|| {});
                    let abort = tokio::spawn(async {}).abort_handle();
                    Some(SignalWorker { thread, abort })
                }
            })
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.begin(1, &tx);
        handle.flush().await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(session.is_active());
        while rx.try_recv().is_ok() {}

        handle.cancel(&tx);
        handle.begin(2, &tx);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "the next link waits for cancel cleanup"
        );
        assert!(rx.try_recv().is_err(), "no status until the worker ends");

        release_tx.send(()).expect("release");
        handle.flush().await;
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert!(session.is_active(), "the new attempt stays active");

        let mut saw_unlinked = false;
        let mut saw_connecting = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AdapterEvent::Account {
                    state: AccountState::Unlinked,
                    ..
                } => {
                    assert!(!saw_connecting, "cleanup must finish before the new link");
                    saw_unlinked = true;
                }
                AdapterEvent::Status {
                    status: AdapterStatus::Connecting,
                    ..
                } => {
                    assert!(saw_unlinked, "Connecting follows the cancel cleanup");
                    saw_connecting = true;
                }
                AdapterEvent::Status {
                    protocol: ProtocolId::Signal,
                    status: AdapterStatus::Stubbed,
                    ..
                } => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert!(saw_unlinked && saw_connecting);
    }

    fn idle_worker() -> SignalWorker {
        let thread = std::thread::spawn(|| {});
        let abort = tokio::spawn(async {}).abort_handle();
        SignalWorker { thread, abort }
    }

    fn counting_delete(calls: &Arc<AtomicUsize>, result: bool) -> DeleteFn {
        let calls = Arc::clone(calls);
        Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            result
        })
    }

    /// The user unlinks a linked account: the worker gets the request for
    /// Signal, the live session is deleted after the worker ended, and the
    /// row leaves the linked state with a normal status line. No
    /// `AccountEnded` goes out: the phone did not remove the device.
    #[tokio::test]
    async fn an_unlink_asks_the_worker_and_deletes_the_session() {
        let session = Arc::new(Session::new());
        let deletes = Arc::new(AtomicUsize::new(0));
        let handle = Handle::spawn_with_delete(
            Arc::clone(&session),
            Box::new(|session, _events| {
                let (_token, _wake) = session.begin_attempt();
                session.mark_active();
                Some(idle_worker())
            }),
            counting_delete(&deletes, true),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.begin(1, &tx);
        handle.flush().await;
        // The worker of this test only records the job and confirms it.
        let mut jobs = session.install_jobs_for_test();
        let asked = Arc::new(AtomicBool::new(false));
        let asked_by_worker = Arc::clone(&asked);
        tokio::spawn(async move {
            if let Some(super::super::live::WorkerJob::Unlink(done)) = jobs.recv().await {
                asked_by_worker.store(true, Ordering::SeqCst);
                let _ = done.send(true);
            }
        });
        while rx.try_recv().is_ok() {}

        handle.unlink(&tx);
        handle.flush().await;
        assert!(asked.load(Ordering::SeqCst), "the worker asked Signal");
        assert_eq!(deletes.load(Ordering::SeqCst), 1);
        assert!(!session.is_active());
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            matches!(
                events.as_slice(),
                [
                    AdapterEvent::Account {
                        protocol: ProtocolId::Signal,
                        state: AccountState::Unlinked,
                    },
                    AdapterEvent::Status {
                        status: AdapterStatus::Stubbed,
                        detail,
                        ..
                    },
                ] if detail == super::super::live::UNLINKED
            ),
            "{events:?}"
        );
    }

    /// No worker runs (for example after a denied key read): nothing can
    /// ask Signal, and the saved session is deleted.
    #[tokio::test]
    async fn an_unlink_with_no_worker_deletes_the_saved_session() {
        let session = Arc::new(Session::new());
        let deletes = Arc::new(AtomicUsize::new(0));
        let handle = Handle::spawn_with_delete(
            Arc::clone(&session),
            Box::new(|_session, _events| None),
            counting_delete(&deletes, true),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.unlink(&tx);
        handle.flush().await;
        assert_eq!(deletes.load(Ordering::SeqCst), 1);
        assert!(matches!(
            rx.try_recv(),
            Ok(AdapterEvent::Account {
                state: AccountState::Unlinked,
                ..
            })
        ));
    }

    /// Files that do not go: the row still leaves the linked state, and
    /// the status line says that the files are still there.
    #[tokio::test]
    async fn an_unlink_that_cannot_delete_says_so() {
        let session = Arc::new(Session::new());
        let deletes = Arc::new(AtomicUsize::new(0));
        let handle = Handle::spawn_with_delete(
            Arc::clone(&session),
            Box::new(|_session, _events| None),
            counting_delete(&deletes, false),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.unlink(&tx);
        handle.flush().await;
        let mut details = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::Status {
                status: AdapterStatus::Error,
                detail,
                ..
            } = event
            {
                details.push(detail);
            }
        }
        assert_eq!(details, [super::super::live::UNLINK_FILES_LEFT]);
    }

    #[tokio::test]
    async fn a_worker_that_fails_to_spawn_reports_the_error_before_unlinked() {
        let session = Arc::new(Session::new());
        let handle = Handle::spawn(
            Arc::clone(&session),
            Box::new(|session, _events| {
                session.mark_active();
                None
            }),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.begin(1, &tx);
        handle.flush().await;
        assert!(!session.is_active());
        let mut order = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                AdapterEvent::Account {
                    state: AccountState::Unlinked,
                    ..
                } => order.push("unlinked"),
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    detail,
                    ..
                } => {
                    assert!(
                        detail.contains("could not start"),
                        "worker-start failure detail, got {detail}"
                    );
                    order.push("error");
                }
                AdapterEvent::Status {
                    status: AdapterStatus::Connecting,
                    ..
                } => order.push("connecting"),
                AdapterEvent::AccountEnded { .. } => {
                    panic!("a worker that failed to start does not end the account")
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(
            order,
            ["connecting", "error", "unlinked"],
            "Connecting shows while the worker starts, then the error, then Unlinked"
        );
    }

    /// The row shows Connecting before the worker closure runs, so a store
    /// migration on that thread cannot hide the row on the old status.
    #[tokio::test]
    async fn connecting_is_queued_before_the_worker_starts() {
        let session = Arc::new(Session::new());
        let (tx, rx) = mpsc::unbounded_channel();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let rx_for_start = Arc::clone(&rx);
        let saw = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_for_start = Arc::clone(&saw);
        let handle = Handle::spawn(Arc::clone(&session), {
            Box::new(move |session, _events| {
                let mut guard = rx_for_start.lock().expect("events");
                while let Ok(event) = guard.try_recv() {
                    if matches!(
                        event,
                        AdapterEvent::Status {
                            status: AdapterStatus::Connecting,
                            ..
                        }
                    ) {
                        saw_for_start.store(true, Ordering::SeqCst);
                    }
                }
                drop(guard);
                let (_token, _wake) = session.begin_attempt();
                let thread = std::thread::spawn(|| {});
                let abort = tokio::spawn(async {}).abort_handle();
                Some(SignalWorker { thread, abort })
            })
        });
        handle.begin(9, &tx);
        handle.flush().await;
        assert!(
            saw.load(Ordering::SeqCst),
            "Connecting is queued before the worker starts"
        );
        assert_eq!(session.pairing(), 9);
        handle.retry_key(&tx);
        handle.flush().await;
        assert_eq!(
            session.pairing(),
            9,
            "a key retry keeps the pairing id and does not start a new link"
        );
    }

    /// The worker has ended after a failed move. Retry starts it again.
    /// That start is the open, and a wrong key there tries the move again.
    #[tokio::test]
    async fn a_retry_after_the_worker_ends_opens_again() {
        let session = Arc::new(Session::new());
        let starts = Arc::new(AtomicUsize::new(0));
        let starts_for_start = Arc::clone(&starts);
        let handle = Handle::spawn(Arc::clone(&session), {
            Box::new(move |session, _events| {
                let (_token, _wake) = session.begin_attempt();
                session.mark_active();
                starts_for_start.fetch_add(1, Ordering::SeqCst);
                let thread = std::thread::spawn(|| {});
                let abort = tokio::spawn(async {}).abort_handle();
                Some(SignalWorker { thread, abort })
            })
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.begin(4, &tx);
        handle.flush().await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        while rx.try_recv().is_ok() {}
        handle.retry_key(&tx);
        handle.flush().await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            2,
            "retry opens the saved session again"
        );
        assert_eq!(session.pairing(), 4, "retry does not start a new link");
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    AdapterEvent::Account {
                        state: AccountState::Unlinked,
                        ..
                    }
                ),
                "retry does not drop the saved account: {event:?}"
            );
        }
    }

    /// A remembered notice asks the worker to load a registration. A failed
    /// start stays quiet: it is not a link the user just started.
    #[tokio::test]
    async fn a_resume_loads_only_a_registered_session() {
        let session = Arc::new(Session::new());
        let saw = Arc::new(AtomicBool::new(false));
        let saw_in_start = Arc::clone(&saw);
        let handle = Handle::spawn(Arc::clone(&session), {
            Box::new(move |session, _events| {
                saw_in_start.store(session.resume_only(), Ordering::SeqCst);
                let _ = session.begin_attempt();
                None
            })
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle.resume(&tx);
        handle.flush().await;
        assert!(saw.load(Ordering::SeqCst), "resume does not provision");
        assert!(rx.try_recv().is_err(), "no link-failure status");

        session.set_resume_only(true);
        let provisioning = Arc::new(AtomicBool::new(true));
        let provisioning_in_start = Arc::clone(&provisioning);
        let handle = Handle::spawn(Arc::clone(&session), {
            Box::new(move |session, _events| {
                provisioning_in_start.store(session.resume_only(), Ordering::SeqCst);
                let _ = session.begin_attempt();
                session.mark_active();
                let thread = std::thread::spawn(|| {});
                let abort = tokio::spawn(async {}).abort_handle();
                Some(SignalWorker { thread, abort })
            })
        });
        handle.begin(1, &tx);
        handle.flush().await;
        assert!(
            !provisioning.load(Ordering::SeqCst),
            "a link the user starts can provision"
        );
    }
}
