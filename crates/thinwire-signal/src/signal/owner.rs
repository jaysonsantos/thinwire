// SPDX-License-Identifier: AGPL-3.0-only
//! One task owns a Signal link attempt.
//!
//! Begin, Cancel, Shutdown, and worker results are messages. The task handles
//! them one at a time. A new Begin runs only after the previous cleanup
//! finished, so a late shutdown cannot clear the next attempt.

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
    Cancel {
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

/// Adapter side. Every method only sends a message.
pub(super) struct Handle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Handle {
    pub(super) fn spawn(session: Arc<Session>, start: StartFn) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let owner = Owner {
            session,
            start,
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

    pub(super) fn cancel(&self, events: &EventTx) {
        let _ = self.tx.send(Msg::Cancel {
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
    worker: Option<SignalWorker>,
    generation: u64,
}

impl Owner {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Begin { pairing, events } => self.begin(pairing, &events).await,
                Msg::Cancel { events } => self.cancel(&events).await,
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
        self.session.set_pairing(pairing);
        self.worker = (self.start)(&self.session, events);
        self.generation = self.session.generation();
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Connecting,
            "Signal linking was queued on the worker. This build is local only.",
        );
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
        self.session.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
