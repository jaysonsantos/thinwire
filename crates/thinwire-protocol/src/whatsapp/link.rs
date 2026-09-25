//! One owner for the WhatsApp link lifecycle.
//!
//! A single tokio task ([`Owner`]) owns the link generation, the bot handle,
//! the stale-store mark, the store open, and the store delete. Every
//! transition is a [`Msg`] on one channel, and the owner runs the messages
//! one at a time, in order. Client callbacks only send a message with their
//! generation; the owner drops a callback of an old generation.
//!
//! Platform work is behind [`LinkBackend`]. The live backend (feature
//! `whatsapp-web`) wraps whatsapp-rust. Tests use a fake backend.
//!
//! State diagram: `team/whatsapp.md`, section "Link lifecycle owner".

#![cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::session::{LinkEvent, Session, WhatsAppSender};
use crate::adapter::{AdapterStatus, EventTx, ProtocolId, emit_status};

/// Longest wait of [`LinkHandle::shutdown`]. Below the 5 s close limit of
/// the app, so `Stopped` still fits (#44).
pub(super) const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

const PAIRING_RUNNING: &str =
    "Experimental WhatsApp pairing is running on the worker. This is not a supported messenger.";
const DATA_DIR_MISSING: &str =
    "Platform app-data directory is unavailable. WhatsApp pairing did not start.";
const STORE_FAILED: &str =
    "WhatsApp device store could not be opened under app-data. Nothing was logged.";
const BUILD_FAILED: &str =
    "WhatsApp pairing client could not be built. No session material was logged.";

/// Why a start failed. No path, OS text, or session material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartError {
    DataDir,
    Store,
    Build,
}

impl StartError {
    const fn detail(self) -> &'static str {
        match self {
            Self::DataDir => DATA_DIR_MISSING,
            Self::Store => STORE_FAILED,
            Self::Build => BUILD_FAILED,
        }
    }
}

/// A started client: its handle for the owner, its sender for the session.
pub(super) struct Started<B> {
    pub bot: B,
    pub sender: Arc<dyn WhatsAppSender>,
}

/// Platform work of the link. Only the owner calls it, one call at a time.
pub(super) trait LinkBackend: Send + Sync + 'static {
    type Bot: Send + 'static;

    /// Open the device store, build the client, and spawn it. Client events
    /// go to `callbacks`.
    fn start(
        &self,
        generation: u64,
        phone: Option<String>,
        callbacks: Callbacks,
    ) -> impl Future<Output = Result<Started<Self::Bot>, StartError>> + Send;

    /// Stop a client and close its store.
    fn stop(&self, bot: Self::Bot) -> impl Future<Output = ()> + Send;

    /// Delete the device store and its SQLite side files.
    fn delete_store(&self) -> impl Future<Output = Result<(), ()>> + Send;
}

enum Msg {
    Begin {
        phone: Option<String>,
    },
    Client {
        generation: u64,
        event: LinkEvent,
    },
    Cancel,
    Shutdown {
        done: oneshot::Sender<()>,
    },
    #[cfg(test)]
    Flush {
        done: oneshot::Sender<()>,
    },
}

/// Sends the client events of one generation to the owner.
///
/// Holds a weak sender, so a running client does not keep the owner alive.
#[derive(Clone)]
pub(super) struct Callbacks {
    generation: u64,
    tx: mpsc::WeakUnboundedSender<Msg>,
}

impl Callbacks {
    pub(super) fn send(&self, event: LinkEvent) {
        if let Some(tx) = self.tx.upgrade() {
            let _ = tx.send(Msg::Client {
                generation: self.generation,
                event,
            });
        }
    }
}

/// The adapter side of the owner. Every method only sends a message.
pub(super) struct LinkHandle {
    tx: mpsc::UnboundedSender<Msg>,
    active: Arc<AtomicBool>,
}

impl LinkHandle {
    /// Spawn the owner task on the current tokio runtime.
    pub(super) fn spawn<B: LinkBackend>(backend: B, session: Session, events: EventTx) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let active = Arc::new(AtomicBool::new(false));
        let owner = Owner {
            backend,
            session,
            events,
            callbacks: tx.downgrade(),
            generation: 0,
            bot: None,
            stale: false,
            active: Arc::clone(&active),
        };
        tokio::spawn(owner.run(rx));
        Self { tx, active }
    }

    /// Start a new pairing. A running link stops first.
    pub(super) fn begin(&self, phone: Option<String>) {
        self.active.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Begin { phone });
    }

    /// Stop the link. Later callbacks of the old generation are dropped.
    pub(super) fn cancel(&self) {
        let _ = self.tx.send(Msg::Cancel);
    }

    /// A pairing was asked for or a client runs.
    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop the link and wait until the owner confirms, at most
    /// [`SHUTDOWN_WAIT`]. `true` means that no client runs.
    pub(super) async fn shutdown(&self) -> bool {
        self.shutdown_within(SHUTDOWN_WAIT).await
    }

    pub(super) async fn shutdown_within(&self, limit: Duration) -> bool {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Shutdown { done }).is_err() {
            return true;
        }
        matches!(tokio::time::timeout(limit, wait).await, Ok(Ok(())))
    }

    /// Wait until the owner ran every message sent before this call.
    #[cfg(test)]
    pub(super) async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Flush { done }).is_ok() {
            let _ = wait.await;
        }
    }
}

struct Owner<B: LinkBackend> {
    backend: B,
    session: Session,
    events: EventTx,
    callbacks: mpsc::WeakUnboundedSender<Msg>,
    /// The only generation whose callbacks count. Starts at 0 (no link).
    generation: u64,
    bot: Option<B::Bot>,
    /// A logout revoked the store, and the delete did not succeed yet.
    stale: bool,
    active: Arc<AtomicBool>,
}

impl<B: LinkBackend> Owner<B> {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Begin { phone } => self.begin(phone).await,
                Msg::Client { generation, event } => self.client(generation, event).await,
                Msg::Cancel => self.stop().await,
                Msg::Shutdown { done } => {
                    self.stop().await;
                    let _ = done.send(());
                }
                #[cfg(test)]
                Msg::Flush { done } => {
                    let _ = done.send(());
                }
            }
        }
        // Every handle is gone: close the client.
        self.stop().await;
    }

    async fn begin(&mut self, phone: Option<String>) {
        self.generation += 1;
        let generation = self.generation;
        if let Some(bot) = self.bot.take() {
            self.backend.stop(bot).await;
        }
        self.session.begin(generation);
        if self.stale {
            if self.backend.delete_store().await.is_err() {
                // The revoked store is still there. Do not open it; the next
                // Begin tries the delete again.
                self.fail(StartError::Store);
                return;
            }
            self.stale = false;
        }
        let callbacks = Callbacks {
            generation,
            tx: self.callbacks.clone(),
        };
        match self.backend.start(generation, phone, callbacks).await {
            Ok(started) => {
                self.bot = Some(started.bot);
                self.session.attach_sender(generation, started.sender);
                self.active.store(true, Ordering::SeqCst);
                self.status(AdapterStatus::Connecting, PAIRING_RUNNING);
            }
            Err(error) => self.fail(error),
        }
    }

    async fn client(&mut self, generation: u64, event: LinkEvent) {
        if generation != self.generation || self.bot.is_none() {
            return;
        }
        if !event.stops_link() {
            self.session.apply(event, generation, &self.events);
            return;
        }
        let invalidate = event.invalidates_device();
        // Later callbacks of this link are stale from here on.
        self.generation += 1;
        self.session.apply(event, generation, &self.events);
        if invalidate {
            self.stale = true;
        }
        if let Some(bot) = self.bot.take() {
            self.backend.stop(bot).await;
        }
        self.active.store(false, Ordering::SeqCst);
        if self.stale && self.backend.delete_store().await.is_ok() {
            self.stale = false;
        }
    }

    async fn stop(&mut self) {
        self.generation += 1;
        if let Some(bot) = self.bot.take() {
            self.backend.stop(bot).await;
        }
        self.active.store(false, Ordering::SeqCst);
    }

    fn fail(&self, error: StartError) {
        self.active.store(false, Ordering::SeqCst);
        self.status(AdapterStatus::Error, error.detail());
    }

    fn status(&self, status: AdapterStatus, detail: &str) {
        emit_status(&self.events, ProtocolId::WhatsApp, status, detail);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Notify;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::adapter::{AdapterEvent, RedactedPairingSecret};
    use crate::whatsapp::session::fake::FakeSender;

    /// Records every backend call in order. The bot is its generation.
    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        callbacks: Mutex<HashMap<u64, Callbacks>>,
        /// When set, `start` waits for a notify before it returns.
        start_gate: Option<Arc<Notify>>,
        /// Number of `delete_store` calls that fail before one succeeds.
        delete_failures: AtomicUsize,
        /// `stop` never returns.
        stop_hangs: bool,
        /// `start` fails with this error.
        start_error: Option<StartError>,
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Fake>);

    impl LinkBackend for Shared {
        type Bot = u64;

        async fn start(
            &self,
            generation: u64,
            _phone: Option<String>,
            callbacks: Callbacks,
        ) -> Result<Started<u64>, StartError> {
            self.0
                .log
                .lock()
                .expect("log")
                .push(format!("start {generation}"));
            self.0
                .callbacks
                .lock()
                .expect("callbacks")
                .insert(generation, callbacks);
            if let Some(gate) = &self.0.start_gate {
                gate.notified().await;
            }
            if let Some(error) = self.0.start_error {
                return Err(error);
            }
            Ok(Started {
                bot: generation,
                sender: Arc::new(FakeSender::default()),
            })
        }

        async fn stop(&self, bot: u64) {
            self.0.log.lock().expect("log").push(format!("stop {bot}"));
            if self.0.stop_hangs {
                std::future::pending::<()>().await;
            }
        }

        async fn delete_store(&self) -> Result<(), ()> {
            let failed = self
                .0
                .delete_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            let entry = if failed { "delete failed" } else { "delete" };
            self.0.log.lock().expect("log").push(entry.into());
            if failed { Err(()) } else { Ok(()) }
        }
    }

    impl Shared {
        fn log(&self) -> Vec<String> {
            self.0.log.lock().expect("log").clone()
        }

        fn callback(&self, generation: u64) -> Callbacks {
            self.0
                .callbacks
                .lock()
                .expect("callbacks")
                .get(&generation)
                .cloned()
                .expect("callbacks of this generation")
        }
    }

    fn owner(fake: Fake) -> (Shared, LinkHandle, Session, UnboundedReceiver<AdapterEvent>) {
        let shared = Shared(Arc::new(fake));
        let (tx, rx) = unbounded_channel();
        let session = Session::default();
        let handle = LinkHandle::spawn(shared.clone(), session.clone(), tx);
        (shared, handle, session, rx)
    }

    fn drain(rx: &mut UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn qr(code: &str) -> LinkEvent {
        LinkEvent::Qr(RedactedPairingSecret::new(code))
    }

    #[tokio::test]
    async fn begin_starts_the_client() {
        let (fake, handle, _session, mut rx) = owner(Fake::default());
        handle.begin(None);
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1"]);
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Connecting,
                ..
            }
        )));
        assert!(handle.is_active());
    }

    /// r4093309818, r4093606395: a repair right after a logout runs after
    /// the delete, never on the revoked store.
    #[tokio::test]
    async fn logout_then_repair_deletes_before_the_new_start() {
        let (fake, handle, _session, _rx) = owner(Fake::default());
        handle.begin(None);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.begin(None);
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "stop 1", "delete", "start 3"]);
    }

    /// r4093192847, r4093309814, r4093726770: callbacks of an old link after
    /// a repair do nothing: no stop, no delete, no session change.
    #[tokio::test]
    async fn late_callbacks_of_an_old_link_are_dropped() {
        let (fake, handle, session, mut rx) = owner(Fake::default());
        handle.begin(None);
        handle.flush().await;
        handle.begin(None);
        handle.flush().await;
        drain(&mut rx);
        let old = fake.callback(1);
        old.send(qr("old-qr"));
        old.send(LinkEvent::Connected);
        old.send(LinkEvent::TemporaryBan);
        old.send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "stop 1", "start 2"]);
        assert!(drain(&mut rx).is_empty(), "no event of the old link");
        assert!(!session.is_connected());
        assert!(handle.is_active(), "the new link keeps running");

        fake.callback(2).send(LinkEvent::Connected);
        handle.flush().await;
        assert!(session.is_connected());
    }

    /// #44 and r4093726770: a cancel during a start stops that client once
    /// the start ends. Callbacks sent after the cancel are dropped.
    #[tokio::test]
    async fn cancel_during_start_stops_the_client_after_the_start() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, session, mut rx) = owner(Fake {
            start_gate: Some(Arc::clone(&gate)),
            ..Fake::default()
        });
        handle.begin(None);
        tokio::task::yield_now().await;
        handle.cancel();
        // The client of generation 1 reports late, after the cancel.
        while fake.0.callbacks.lock().expect("callbacks").is_empty() {
            tokio::task::yield_now().await;
        }
        fake.callback(1).send(LinkEvent::Connected);
        gate.notify_one();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "stop 1"]);
        assert!(!session.is_connected());
        assert!(!handle.is_active());
        assert!(!drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Ready,
                ..
            }
        )));
    }

    /// r4093471561, path.rs:58: a failed delete stays armed. No start opens
    /// the revoked store; the next Begin tries the delete again.
    #[tokio::test]
    async fn failed_delete_stays_armed_and_blocks_the_start() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            delete_failures: AtomicUsize::new(2),
            ..Fake::default()
        });
        handle.begin(None);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        drain(&mut rx);
        handle.begin(None);
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == STORE_FAILED
        )));
        assert!(!handle.is_active());
        handle.begin(None);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "stop 1",
                "delete failed",
                "delete failed",
                "delete",
                "start 4"
            ]
        );
    }

    #[tokio::test]
    async fn failed_start_reports_the_reason_and_runs_nothing() {
        for (error, detail) in [
            (StartError::DataDir, DATA_DIR_MISSING),
            (StartError::Store, STORE_FAILED),
            (StartError::Build, BUILD_FAILED),
        ] {
            let (fake, handle, session, mut rx) = owner(Fake {
                start_error: Some(error),
                ..Fake::default()
            });
            handle.begin(None);
            handle.flush().await;
            assert!(drain(&mut rx).iter().any(|event| matches!(
                event,
                AdapterEvent::Status { status: AdapterStatus::Error, detail: text, .. } if text == detail
            )));
            assert!(!handle.is_active());
            // A late callback of the failed start does nothing.
            fake.callback(1).send(LinkEvent::Connected);
            handle.flush().await;
            assert!(!session.is_connected());
            assert!(handle.shutdown().await);
            assert_eq!(fake.log(), vec!["start 1"], "nothing to stop");
        }
    }

    #[tokio::test]
    async fn ban_and_dead_pairing_stop_without_a_delete() {
        for event in [
            LinkEvent::TemporaryBan,
            LinkEvent::PairFailed,
            LinkEvent::PairThrottled,
            LinkEvent::QrExhausted,
        ] {
            let (fake, handle, session, _rx) = owner(Fake::default());
            handle.begin(None);
            handle.flush().await;
            fake.callback(1).send(event);
            handle.flush().await;
            handle.begin(None);
            handle.flush().await;
            assert_eq!(fake.log(), vec!["start 1", "stop 1", "start 3"]);
            assert!(session.stopped().is_none(), "Begin clears the stop reason");
        }
    }

    /// Interleave logout, cancel, repair, and late callbacks of every older
    /// link. Only the last link runs, and the store is deleted once.
    #[tokio::test]
    async fn logout_cancel_repair_and_late_callbacks_interleave() {
        let (fake, handle, session, mut rx) = owner(Fake::default());
        handle.begin(None); // generation 1
        handle.flush().await;
        let first = fake.callback(1);
        first.send(LinkEvent::LoggedOut); // generation 2
        handle.cancel(); // generation 3
        first.send(LinkEvent::Connected);
        handle.begin(None); // generation 4
        first.send(qr("stale"));
        first.send(LinkEvent::LoggedOut);
        handle.flush().await;
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AdapterEvent::WhatsAppQr { .. })),
            "a QR of a canceled link must not reach the shell"
        );
        assert_eq!(fake.log(), vec!["start 1", "stop 1", "delete", "start 4"]);
        assert!(!session.is_connected());

        fake.callback(4).send(qr("fresh"));
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::WhatsAppQr { generation: 4, code } if code.reveal() == "fresh"
        )));
    }

    /// #44: shutdown waits for a pending start, then stops that client.
    #[tokio::test]
    async fn shutdown_waits_for_a_pending_start_then_stops_it() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, _session, _rx) = owner(Fake {
            start_gate: Some(Arc::clone(&gate)),
            ..Fake::default()
        });
        handle.begin(None);
        let shutdown = handle.shutdown();
        let mut shutdown = std::pin::pin!(shutdown);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), shutdown.as_mut())
                .await
                .is_err(),
            "shutdown must wait for the start"
        );
        gate.notify_one();
        assert!(shutdown.await);
        assert_eq!(fake.log(), vec!["start 1", "stop 1"]);
        assert!(!handle.is_active());
    }

    /// #44: the shutdown wait has its own bound below the app close limit.
    #[tokio::test]
    async fn shutdown_is_bounded_when_the_client_never_stops() {
        let (_fake, handle, _session, _rx) = owner(Fake {
            stop_hangs: true,
            ..Fake::default()
        });
        handle.begin(None);
        handle.flush().await;
        let started = std::time::Instant::now();
        assert!(!handle.shutdown_within(Duration::from_millis(50)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(SHUTDOWN_WAIT < Duration::from_secs(5));
    }
}
