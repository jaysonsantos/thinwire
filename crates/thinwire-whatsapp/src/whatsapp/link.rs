// SPDX-License-Identifier: AGPL-3.0-only
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

use super::inbox::{InboxStore, SavedHashes, SavedInbox};
use super::session::{LinkEvent, Session, WhatsAppSender, account};
use thinwire_protocol::{
    AccountState, AdapterEvent, AdapterStatus, EventTx, ProtocolId, StoreAccessReason, emit_status,
};

/// Longest wait of [`LinkHandle::shutdown`]. Below the 5 s close limit of
/// the app, so `Stopped` still fits (#44).
pub(super) const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

/// How often the chat cache takes the changes of a linked session.
pub(super) const CACHE_FLUSH: Duration = Duration::from_secs(5);

/// Longest wait of a shutdown for the last cache write. Below
/// [`SHUTDOWN_WAIT`], with room for the client stop.
const CACHE_FLUSH_WAIT: Duration = Duration::from_millis(500);

/// Longest wait of the owner for one client stop. Below [`SHUTDOWN_WAIT`],
/// so a shutdown still gets its answer in time (#98).
pub(super) const STOP_WAIT: Duration = Duration::from_secs(2);

const PAIRING_RUNNING: &str =
    "Experimental WhatsApp pairing is running on the worker. This is not a supported messenger.";
const DATA_DIR_MISSING: &str =
    "Platform app-data directory is unavailable. WhatsApp pairing did not start.";
const STORE_FAILED: &str =
    "WhatsApp device store could not be opened under app-data. Nothing was logged.";
const REVOKE_NOT_SAVED: &str = "WhatsApp unlinked this device, but thinwire could not remove or mark its session file. Check that the app-data folder is writable, then pair again: the next pairing removes the old session first.";
const STOP_PENDING: &str = "The WhatsApp client did not stop in time. thinwire keeps stopping it. A new pairing waits until it stopped.";
const STILL_RUNNING: &str = "The last WhatsApp client is still stopping. Wait a moment, then start pairing again. If this stays, restart thinwire.";
const STOP_ENDED: &str =
    "The last WhatsApp client stopped. No linked-device session is running. You can pair again.";
const BUILD_FAILED: &str =
    "WhatsApp pairing client could not be built. No session material was logged.";
const NOT_PAIRED: &str = "WhatsApp has no linked device.";
/// The status line after the user unlinked the account.
pub(super) const UNLINKED: &str = "WhatsApp is unlinked. The session on this computer was deleted.";

/// Longest wait of an unlink for the answer of WhatsApp to the log out.
/// The unlink then continues: this computer forgets the device in each case.
const LOG_OUT_WAIT: Duration = Duration::from_secs(5);

/// Pairing id of a resume at app start. The shell has no pairing with this
/// id, so it drops any QR code or pair code of a resume.
const RESUME_PAIRING: u64 = 0;

/// Why a start failed. No path, OS text, or session material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StartError {
    DataDir,
    Store,
    Build,
    /// An earlier client did not stop yet. Two clients on one store break it.
    StillRunning,
    /// The keychain refused the read. The store stays where it is.
    /// `saved` is true when `device.sqlite` is already on disk.
    KeychainDenied {
        saved: bool,
    },
    /// The encrypted store did not open. The folder was moved aside.
    WrongKey,
    /// The store files could not be moved aside. They stay. Nothing was deleted.
    MoveFailed,
    /// The keychain or the store encryption stopped the link. The text is the
    /// account-row sentence. It does not contain the key.
    #[cfg(feature = "whatsapp-web")]
    Reported(String),
    /// A resume found no paired device in the store. No client started.
    NotPaired,
}

impl StartError {
    fn detail(&self) -> &str {
        match self {
            Self::DataDir => DATA_DIR_MISSING,
            Self::Store => STORE_FAILED,
            Self::Build => BUILD_FAILED,
            Self::StillRunning => STILL_RUNNING,
            Self::KeychainDenied { .. } => thinwire_protocol::KEYCHAIN_ACCESS_DENIED,
            Self::WrongKey => thinwire_protocol::whatsapp_session_kept(),
            Self::MoveFailed => thinwire_protocol::whatsapp_key_wrong_kept(),
            #[cfg(feature = "whatsapp-web")]
            Self::Reported(text) => text,
            Self::NotPaired => NOT_PAIRED,
        }
    }
}

/// A started client: its handle for the owner, its sender for the session.
pub(super) struct Started<B> {
    pub bot: B,
    pub sender: Arc<dyn WhatsAppSender>,
    /// The chat list that the disk kept for this device. Empty for a new
    /// pairing.
    pub saved: SavedInbox,
    /// Where the chat list of this session goes. `None`: no cache.
    pub cache: Option<Arc<dyn InboxStore>>,
}

/// Writes the changed chats of one session to its cache.
pub(super) struct CacheWriter {
    store: Arc<dyn InboxStore>,
    saved: std::sync::Mutex<SavedHashes>,
}

impl CacheWriter {
    fn new(store: Arc<dyn InboxStore>, saved: SavedHashes) -> Self {
        Self {
            store,
            saved: std::sync::Mutex::new(saved),
        }
    }

    /// Write what changed since the last write. Blocking: call it off the
    /// async workers. A failed write tries the same changes next time.
    pub(super) fn flush(&self, session: &Session) {
        let mut saved = self
            .saved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changes = session.with_inbox(|inbox| inbox.changes(&saved));
        if changes.is_empty() {
            return;
        }
        if self.store.save(&changes).is_ok() {
            saved.apply(&changes);
        } else {
            tracing::warn!("whatsapp chat cache write failed");
        }
    }
}

/// Platform work of the link. Only the owner calls it, one call at a time.
/// `Clone`: a stop runs on its own task, so the owner can bound it.
pub(super) trait LinkBackend: Clone + Send + Sync + 'static {
    type Bot: Send + 'static;

    /// Open the device store, build the client, and spawn it. Client events
    /// go to `callbacks`. With `resume`, a store with no paired device
    /// returns [`StartError::NotPaired`] before any client is built.
    fn start(
        &self,
        generation: u64,
        phone: Option<String>,
        callbacks: Callbacks,
        resume: bool,
    ) -> impl Future<Output = Result<Started<Self::Bot>, StartError>> + Send;

    /// A device store file is on disk.
    fn has_store(&self) -> impl Future<Output = bool> + Send;

    /// Stop a client and close its store.
    fn stop(&self, bot: Self::Bot) -> impl Future<Output = ()> + Send;

    /// Ask WhatsApp to remove this device from the account. `true` when
    /// WhatsApp confirmed it. The client is not usable after this call:
    /// [`Self::stop`] follows.
    fn log_out(&self, bot: &Self::Bot) -> impl Future<Output = bool> + Send;

    /// Delete the device store, its SQLite side files, and the revoked mark.
    fn delete_store(&self) -> impl Future<Output = Result<(), ()>> + Send;

    /// Record on disk that the phone revoked the store. The mark survives a
    /// restart; [`Self::delete_store`] removes it.
    /// `Err` means that the mark is not on disk (for example a read-only
    /// app-data folder).
    fn mark_revoked(&self) -> impl Future<Output = Result<(), ()>> + Send;

    /// The store carries a revoked mark, maybe from an earlier run.
    fn is_revoked(&self) -> impl Future<Output = bool> + Send;
}

enum Msg {
    Begin {
        phone: Option<String>,
        /// The shell's id for this pairing (`WhatsAppBeginLink`). Every QR
        /// and pair code of it carries this number (ADR 0010 rule 8).
        pairing: u64,
    },
    /// Read the session key again. This is not a new link.
    Retry,
    /// The app started with a saved gate acceptance. Open a saved, paired
    /// session. A new link still waits for `Begin`.
    Resume,
    Client {
        generation: u64,
        event: LinkEvent,
    },
    Cancel,
    /// The user unlinks the account: log out, stop, and delete the store.
    Unlink,
    Shutdown {
        done: oneshot::Sender<()>,
    },
    /// A client stop ended. `stop` is its number.
    StopEnded {
        stop: u64,
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
        Self::spawn_with_stop_wait(backend, session, events, STOP_WAIT)
    }

    /// [`Self::spawn`] with another bound for one client stop.
    pub(super) fn spawn_with_stop_wait<B: LinkBackend>(
        backend: B,
        session: Session,
        events: EventTx,
        stop_wait: Duration,
    ) -> Self {
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
            stop_wait,
            stops: 0,
            stopping: None,
            last_phone: None,
            last_pairing: None,
            last_resume: false,
            cache: None,
        };
        tokio::spawn(owner.run(rx));
        Self { tx, active }
    }

    /// Start a new pairing. A running link stops first.
    pub(super) fn begin(&self, phone: Option<String>, pairing: u64) {
        self.active.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Begin { phone, pairing });
    }

    /// Read the session key again. Does not delete the store.
    pub(super) fn retry(&self) {
        let _ = self.tx.send(Msg::Retry);
    }

    /// Open the saved session, if one is on disk and paired. No QR code.
    pub(super) fn resume(&self) {
        let _ = self.tx.send(Msg::Resume);
    }

    /// Stop the link. Later callbacks of the old generation are dropped.
    pub(super) fn cancel(&self) {
        let _ = self.tx.send(Msg::Cancel);
    }

    /// The user unlinks the account. The owner logs the device out, stops
    /// the client, deletes the device store, and sends `Unlinked`.
    pub(super) fn unlink(&self) {
        let _ = self.tx.send(Msg::Unlink);
    }

    /// A sender for client events of `generation`, for code outside the
    /// client (for example a send result that shows a revoked device).
    pub(super) fn callbacks(&self, generation: u64) -> Callbacks {
        Callbacks {
            generation,
            tx: self.tx.downgrade(),
        }
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
    /// Longest wait for one client stop ([`STOP_WAIT`]).
    stop_wait: Duration,
    /// Number of the last client stop.
    stops: u64,
    /// A stop that did not end within `stop_wait`, with its number. It keeps
    /// running; no new client starts before it ends (#98).
    stopping: Option<(u64, tokio::task::JoinHandle<()>)>,
    /// Phone and pairing of the last Begin. A key retry uses these again.
    last_phone: Option<String>,
    last_pairing: Option<u64>,
    /// The last start was a resume. A key retry is a resume again.
    last_resume: bool,
    /// The chat cache of the running client.
    cache: Option<Arc<CacheWriter>>,
}

impl<B: LinkBackend> Owner<B> {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Begin { phone, pairing } => self.begin(phone, pairing, true, false).await,
                Msg::Retry => self.retry_key().await,
                Msg::Resume => self.resume().await,
                Msg::Client { generation, event } => self.client(generation, event).await,
                Msg::Cancel => {
                    self.stop().await;
                    // The pairing is over. A later gate acceptance resumes
                    // a saved store again.
                    self.last_phone = None;
                    self.last_pairing = None;
                    self.last_resume = false;
                }
                Msg::Unlink => self.unlink().await,
                Msg::Shutdown { done } => {
                    // The chat list goes to disk before the client stops.
                    flush_cache(self.cache.clone(), self.session.clone()).await;
                    // Confirm only a client that really stopped. With no
                    // answer the adapter sends an error, not `Stopped`.
                    if self.stop().await && !self.still_stopping() {
                        let _ = done.send(());
                    }
                }
                Msg::StopEnded { stop } => self.stop_ended(stop).await,
                #[cfg(test)]
                Msg::Flush { done } => {
                    let _ = done.send(());
                }
            }
        }
        // Every handle is gone: close the client.
        let _ = self.stop().await;
    }

    /// Read the key again with the saved phone and pairing.
    /// This does not delete the store and does not start a new shell link.
    /// The new open tries the move-aside again when the key is wrong.
    async fn retry_key(&mut self) {
        let Some(pairing) = self.last_pairing else {
            // No pairing id yet. A store file on disk is still a saved account.
            let saved = super::path::whatsapp_device_store_path().is_ok_and(|path| path.is_file());
            self.fail(StartError::KeychainDenied { saved });
            return;
        };
        let phone = self.last_phone.clone();
        let resume = self.last_resume;
        self.begin(phone, pairing, false, resume).await;
    }

    /// Open the saved session at app start. A link that already runs or was
    /// asked for stays as it is. No store, a revoked store, and a store with
    /// no paired device start no client, so no QR code goes out.
    async fn resume(&mut self) {
        if self.bot.is_some() || self.last_pairing.is_some() || self.still_stopping() {
            return;
        }
        if self.stale || self.backend.is_revoked().await || !self.backend.has_store().await {
            return;
        }
        self.begin(None, RESUME_PAIRING, false, true).await;
    }

    async fn begin(
        &mut self,
        phone: Option<String>,
        pairing: u64,
        allow_delete: bool,
        resume: bool,
    ) {
        self.last_phone.clone_from(&phone);
        self.last_pairing = Some(pairing);
        self.last_resume = resume;
        self.generation += 1;
        let generation = self.generation;
        // A client that did not stop yet still holds the store (#98).
        if self.still_stopping() {
            self.fail(StartError::StillRunning);
            return;
        }
        if let Some(bot) = self.bot.take()
            && !self.stop_bot(bot).await
        {
            self.fail(StartError::StillRunning);
            return;
        }
        // The old client's cache ends with it. A new start makes its own.
        self.cache = None;
        self.session.begin(generation);
        self.session.set_pairing(pairing);
        // A first login: the shell drops inbox events until Linked.
        let _ = self.events.send(account(AccountState::Linking));
        // The mark on disk covers a delete that failed before a restart.
        if allow_delete && (self.stale || self.backend.is_revoked().await) {
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
        let result = self
            .backend
            .start(generation, phone, callbacks, resume)
            .await;
        // The user can cancel while the start runs: the adapter reset the
        // session and sent Unlinked. Then this start is over, with no late
        // status (the Cancel message waits behind it in the queue).
        if !self.session.is_link(generation) {
            if let Ok(started) = result {
                // A slow stop reports itself and blocks the next start; this
                // start is over either way.
                let _ = self.stop_bot(started.bot).await;
            }
            self.active.store(false, Ordering::SeqCst);
            return;
        }
        match result {
            Ok(started) => {
                self.bot = Some(started.bot);
                // The saved chats load before the first chat page.
                let hashes = self
                    .session
                    .with_inbox(|inbox| inbox.restore(started.saved));
                self.cache = started.cache.map(|store| {
                    let writer = Arc::new(CacheWriter::new(store, hashes));
                    spawn_cache_flush(Arc::clone(&writer), self.session.clone(), generation);
                    writer
                });
                self.session.attach_sender(generation, started.sender);
                self.active.store(true, Ordering::SeqCst);
                self.status(AdapterStatus::Connecting, PAIRING_RUNNING);
            }
            Err(StartError::NotPaired) => {
                // Nothing to open. The next Begin is a normal first link.
                self.last_pairing = None;
                self.last_resume = false;
                self.fail(StartError::NotPaired);
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
        // Later callbacks of this link are stale from here on. A stopped
        // link writes no more chats.
        self.generation += 1;
        self.cache = None;
        self.session.apply(event, generation, &self.events);
        let marked = if invalidate {
            self.stale = true;
            self.backend.mark_revoked().await.is_ok()
        } else {
            true
        };
        let stopped = match self.bot.take() {
            Some(bot) => self.stop_bot(bot).await,
            None => true,
        };
        self.active.store(false, Ordering::SeqCst);
        // A client that still runs holds the store: the next Begin deletes it
        // after the stop ends.
        if self.stale && stopped && self.backend.delete_store().await.is_ok() {
            self.stale = false;
        }
        // Neither the mark nor the delete reached the disk: only memory knows
        // that the store is revoked. Tell the user; a Begin in this run still
        // deletes it first (Codex r4103855622).
        if self.stale && !marked {
            self.status(AdapterStatus::Error, REVOKE_NOT_SAVED);
        }
    }

    /// The user unlinks the account. Ask WhatsApp to remove this device,
    /// end the session, and delete the device store, as after a logout on
    /// the phone. The log out is best effort: with no connection, only this
    /// computer forgets the device, and the phone still lists it.
    async fn unlink(&mut self) {
        // `true` also with no client: there is no device to log out.
        let confirmed = match self.bot.as_ref().map(|bot| self.backend.log_out(bot)) {
            Some(asking) => matches!(tokio::time::timeout(LOG_OUT_WAIT, asking).await, Ok(true)),
            None => true,
        };
        if !confirmed {
            tracing::warn!("whatsapp did not confirm the log out of this device");
        }
        // Later callbacks of this link are stale, also the logout event
        // that the client sends for its own log out.
        self.generation += 1;
        self.cache = None;
        self.last_phone = None;
        self.last_pairing = None;
        self.last_resume = false;
        for event in self.session.reset() {
            let _ = self.events.send(event);
        }
        // The store is not valid any more. The mark covers a delete that
        // fails, and a restart before the delete.
        self.stale = true;
        let marked = self.backend.mark_revoked().await.is_ok();
        let stopped = match self.bot.take() {
            Some(bot) => self.stop_bot(bot).await,
            None => !self.still_stopping(),
        };
        self.active.store(false, Ordering::SeqCst);
        if stopped && self.backend.delete_store().await.is_ok() {
            self.stale = false;
        }
        let _ = self.events.send(account(AccountState::Unlinked));
        if self.stale && !marked {
            self.status(AdapterStatus::Error, REVOKE_NOT_SAVED);
        } else if stopped {
            self.status(AdapterStatus::Stubbed, UNLINKED);
        }
        // A slow stop shows [`STOP_PENDING`]. `stop_ended` deletes the
        // store when that stop ends.
    }

    /// Stop the current client. `false` if it did not stop in time.
    async fn stop(&mut self) -> bool {
        self.cache = None;
        self.generation += 1;
        let stopped = match self.bot.take() {
            Some(bot) => self.stop_bot(bot).await,
            None => true,
        };
        self.active.store(false, Ordering::SeqCst);
        stopped
    }

    /// Stop one client on its own task and wait at most `stop_wait`
    /// (Codex r4104252007). A stop that takes longer keeps running; the owner
    /// reports it and refuses a new start until it ends.
    async fn stop_bot(&mut self, bot: B::Bot) -> bool {
        let backend = self.backend.clone();
        self.stops += 1;
        let stop = self.stops;
        let owner = self.callbacks.clone();
        let mut task = tokio::spawn(async move {
            backend.stop(bot).await;
            // A slow stop clears its error when it ends (#158 review).
            if let Some(owner) = owner.upgrade() {
                let _ = owner.send(Msg::StopEnded { stop });
            }
        });
        if tokio::time::timeout(self.stop_wait, &mut task)
            .await
            .is_ok()
        {
            return true;
        }
        self.stopping = Some((stop, task));
        self.status(AdapterStatus::Error, STOP_PENDING);
        false
    }

    /// A slow stop from before still runs.
    fn still_stopping(&mut self) -> bool {
        match &self.stopping {
            Some((_, task)) if !task.is_finished() => true,
            Some(_) => {
                self.stopping = None;
                false
            }
            None => false,
        }
    }

    /// The slow stop `stop` ended. Delete a revoked store (#167), then
    /// replace [`STOP_PENDING`] with the reason of the stop, or with
    /// "stopped" (#158 review). A stop that ended in time, or one that a
    /// later start already saw end, does nothing.
    async fn stop_ended(&mut self, stop: u64) {
        if !matches!(&self.stopping, Some((pending, _)) if *pending == stop) {
            return;
        }
        self.stopping = None;
        // The client let go of a revoked store: delete it now, not at the
        // next pairing (#167). A Begin still deletes a leftover.
        if self.stale && self.backend.delete_store().await.is_ok() {
            self.stale = false;
        }
        // Neither the delete nor the mark reached the disk. Keep the warning;
        // the stop reason would hide it.
        if self.stale && !self.backend.is_revoked().await {
            self.status(AdapterStatus::Error, REVOKE_NOT_SAVED);
            return;
        }
        match self.session.stopped() {
            Some(reason) => self.status(AdapterStatus::Error, reason),
            None => self.status(AdapterStatus::Stubbed, STOP_ENDED),
        }
    }

    fn fail(&self, error: StartError) {
        self.active.store(false, Ordering::SeqCst);
        match error {
            StartError::KeychainDenied { saved } => {
                // A store file is a linked account. Mark it before the
                // denial so the row stays linked and shows Retry, not Link.
                if saved {
                    let _ = self.events.send(account(AccountState::Linked));
                }
                let _ = self.events.send(AdapterEvent::StoreAccess {
                    protocol: ProtocolId::WhatsApp,
                    reason: StoreAccessReason::KeychainDenied,
                });
            }
            StartError::WrongKey => {
                // The store files are aside. Nothing here deletes them.
                // The account is not linked.
                let _ = self.events.send(AdapterEvent::StoreAccess {
                    protocol: ProtocolId::WhatsApp,
                    reason: StoreAccessReason::WrongKey,
                });
                let _ = self.events.send(account(AccountState::Unlinked));
            }
            StartError::MoveFailed => {
                // The files stayed. This is not a wrong key. The account stays.
                self.status(
                    AdapterStatus::Error,
                    thinwire_protocol::whatsapp_key_wrong_kept(),
                );
            }
            StartError::NotPaired => {
                self.status(AdapterStatus::Stubbed, NOT_PAIRED);
                let _ = self.events.send(account(AccountState::Unlinked));
            }
            other => {
                self.status(AdapterStatus::Error, other.detail());
                let _ = self.events.send(account(AccountState::Unlinked));
            }
        }
    }

    fn status(&self, status: AdapterStatus, detail: &str) {
        emit_status(&self.events, ProtocolId::WhatsApp, status, detail);
    }
}

/// Write the changed chats now, bounded by [`CACHE_FLUSH_WAIT`].
async fn flush_cache(writer: Option<Arc<CacheWriter>>, session: Session) {
    let Some(writer) = writer else {
        return;
    };
    let write = tokio::task::spawn_blocking(move || writer.flush(&session));
    if tokio::time::timeout(CACHE_FLUSH_WAIT, write).await.is_err() {
        tracing::warn!("whatsapp chat cache: the last write did not end in time");
    }
}

/// Write the changed chats every [`CACHE_FLUSH`] while link `generation`
/// runs. The task ends with the link: a logout or a cancel writes nothing
/// more, and a shutdown writes through [`flush_cache`].
fn spawn_cache_flush(writer: Arc<CacheWriter>, session: Session, generation: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CACHE_FLUSH);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if !session.is_link(generation) || session.is_closed() {
                break;
            }
            let writer = Arc::clone(&writer);
            let held = session.clone();
            let _ = tokio::task::spawn_blocking(move || writer.flush(&held)).await;
        }
    });
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Notify;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::whatsapp::session::fake::FakeSender;
    use thinwire_protocol::{AdapterEvent, RedactedPairingSecret};

    /// Records every backend call in order. The bot is its generation.
    #[derive(Default)]
    pub(in crate::whatsapp) struct Fake {
        pub log: Mutex<Vec<String>>,
        pub callbacks: Mutex<HashMap<u64, Callbacks>>,
        /// When set, `start` waits for a notify before it returns.
        pub start_gate: Option<Arc<Notify>>,
        /// Number of `delete_store` calls that fail before one succeeds.
        pub delete_failures: AtomicUsize,
        /// `stop` never returns.
        pub stop_hangs: bool,
        /// `stop` waits for this notify, then returns (a slow stop).
        pub stop_gate: Option<Arc<Notify>>,
        /// `start` fails with this error.
        pub start_error: Option<StartError>,
        /// The revoked mark "on disk". Survives a new owner (a restart).
        pub revoked: AtomicBool,
        /// `mark_revoked` fails (a read-only app-data folder).
        pub mark_fails: bool,
        /// WhatsApp does not confirm a log out (no connection).
        pub log_out_fails: bool,
        /// No device store file is on disk.
        pub no_store: bool,
        /// The store has no paired device. A resume start refuses it.
        pub unpaired: bool,
        /// The chat list that `start` hands back as saved.
        pub saved: Mutex<SavedInbox>,
        /// The cache that `start` hands back.
        pub cache: Option<Arc<FakeCache>>,
    }

    /// Records every cache write.
    #[derive(Default)]
    pub(in crate::whatsapp) struct FakeCache {
        pub writes: Mutex<Vec<crate::whatsapp::inbox::InboxChanges>>,
    }

    impl InboxStore for FakeCache {
        fn save(&self, changes: &crate::whatsapp::inbox::InboxChanges) -> Result<(), ()> {
            self.writes.lock().expect("writes").push(changes.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    pub(in crate::whatsapp) struct Shared(pub Arc<Fake>);

    impl LinkBackend for Shared {
        type Bot = u64;

        async fn start(
            &self,
            generation: u64,
            _phone: Option<String>,
            callbacks: Callbacks,
            resume: bool,
        ) -> Result<Started<u64>, StartError> {
            let entry = if resume { "resume" } else { "start" };
            self.0
                .log
                .lock()
                .expect("log")
                .push(format!("{entry} {generation}"));
            if resume && self.0.unpaired {
                return Err(StartError::NotPaired);
            }
            self.0
                .callbacks
                .lock()
                .expect("callbacks")
                .insert(generation, callbacks);
            if let Some(gate) = &self.0.start_gate {
                gate.notified().await;
            }
            if let Some(error) = self.0.start_error.clone() {
                return Err(error);
            }
            Ok(Started {
                bot: generation,
                sender: Arc::new(FakeSender::default()),
                saved: self.0.saved.lock().expect("saved").clone(),
                cache: self
                    .0
                    .cache
                    .clone()
                    .map(|cache| cache as Arc<dyn InboxStore>),
            })
        }

        async fn stop(&self, bot: u64) {
            self.0.log.lock().expect("log").push(format!("stop {bot}"));
            if self.0.stop_hangs {
                std::future::pending::<()>().await;
            }
            if let Some(gate) = &self.0.stop_gate {
                gate.notified().await;
                self.0
                    .log
                    .lock()
                    .expect("log")
                    .push(format!("stopped {bot}"));
            }
        }

        async fn log_out(&self, bot: &u64) -> bool {
            self.0
                .log
                .lock()
                .expect("log")
                .push(format!("log out {bot}"));
            !self.0.log_out_fails
        }

        async fn delete_store(&self) -> Result<(), ()> {
            // Rust 1.99 renames this to `try_update`. The nix CI shell is 1.98.
            #[allow(deprecated)]
            let failed = self
                .0
                .delete_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            let entry = if failed { "delete failed" } else { "delete" };
            self.0.log.lock().expect("log").push(entry.into());
            if failed {
                Err(())
            } else {
                self.0.revoked.store(false, Ordering::SeqCst);
                Ok(())
            }
        }

        async fn mark_revoked(&self) -> Result<(), ()> {
            if self.0.mark_fails {
                self.0.log.lock().expect("log").push("mark failed".into());
                return Err(());
            }
            self.0.log.lock().expect("log").push("mark revoked".into());
            self.0.revoked.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn is_revoked(&self) -> bool {
            self.0.revoked.load(Ordering::SeqCst)
        }

        async fn has_store(&self) -> bool {
            !self.0.no_store
        }
    }

    impl Shared {
        pub(in crate::whatsapp) fn log(&self) -> Vec<String> {
            self.0.log.lock().expect("log").clone()
        }

        pub(in crate::whatsapp) fn callback(&self, generation: u64) -> Callbacks {
            self.0
                .callbacks
                .lock()
                .expect("callbacks")
                .get(&generation)
                .cloned()
                .expect("callbacks of this generation")
        }
    }

    pub(in crate::whatsapp) fn owner(
        fake: Fake,
    ) -> (Shared, LinkHandle, Session, UnboundedReceiver<AdapterEvent>) {
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
        handle.begin(None, 7);
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
        assert_eq!(
            events.first(),
            Some(&AdapterEvent::Account {
                protocol: ProtocolId::WhatsApp,
                state: AccountState::Linking,
            }),
            "a pairing is Linking until the client connects"
        );
        assert!(handle.is_active());
    }

    /// A resume at app start opens a saved, paired store with no shell
    /// pairing. It links like a normal start.
    #[tokio::test]
    async fn a_resume_opens_a_saved_paired_store() {
        let (fake, handle, session, mut rx) = owner(Fake::default());
        handle.resume();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["resume 1"]);
        assert!(handle.is_active());
        fake.callback(1).send(LinkEvent::Connected);
        handle.flush().await;
        assert!(session.is_connected());
        let events = drain(&mut rx);
        assert!(events.contains(&AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        }));
        // A second resume, or a resume while a link runs, starts nothing.
        handle.resume();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["resume 1"]);
    }

    /// A saved session shows its saved chats when it connects. A shutdown
    /// writes the chats that changed. A logout writes nothing more.
    #[tokio::test]
    async fn saved_chats_come_back_and_a_shutdown_writes_the_changes() {
        use crate::whatsapp::inbox::{SavedChat, SavedMessage};
        let chat = "111@s.whatsapp.net";
        let saved = SavedInbox {
            chats: vec![(
                chat.to_string(),
                SavedChat {
                    name: Some("Ana".into()),
                    unread: 2,
                    timestamp: 9,
                    read_at: None,
                    history_at: 9,
                    mute: crate::whatsapp::inbox::Mute::Off,
                    messages: vec![SavedMessage {
                        id: "m1".into(),
                        from_me: false,
                        sender_name: Some("Ana".into()),
                        sender_jid: Some(chat.into()),
                        body: "saved".into(),
                        timestamp: 9,
                        reactions: Vec::new(),
                        reply_to: None,
                        mentions: Vec::new(),
                    }],
                },
            )],
            ..SavedInbox::default()
        };
        let cache = Arc::new(FakeCache::default());
        let (fake, handle, _session, mut rx) = owner(Fake {
            saved: Mutex::new(saved),
            cache: Some(Arc::clone(&cache)),
            ..Fake::default()
        });
        handle.resume();
        handle.flush().await;
        fake.callback(1).send(LinkEvent::Connected);
        handle.flush().await;
        let rows: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|event| match event {
                AdapterEvent::ConversationUpsert { conversation } => Some(conversation),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 1, "the saved chat shows");
        assert_eq!(rows[0].title, "Ana");
        assert_eq!(rows[0].unread, 2);

        // Nothing changed: the shutdown has nothing to write.
        fake.callback(1).send(LinkEvent::Messages(vec![
            crate::whatsapp::inbox::tests::message(chat, "m2", "new", 10),
        ]));
        handle.flush().await;
        assert!(handle.shutdown().await);
        let writes = cache.writes.lock().expect("writes").clone();
        assert_eq!(writes.len(), 1, "one write at shutdown");
        assert_eq!(writes[0].chats.len(), 1);
        assert_eq!(
            writes[0].chats[0].2.messages.len(),
            2,
            "the new message too"
        );
    }

    /// A Cancel ends the pairing. The next gate acceptance resumes the
    /// saved store again.
    #[tokio::test]
    async fn a_resume_after_a_cancel_opens_the_saved_store() {
        let (fake, handle, _session, _rx) = owner(Fake::default());
        handle.begin(None, 3);
        handle.flush().await;
        handle.cancel();
        handle.flush().await;
        handle.resume();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "stop 1", "resume 3"]);
        assert!(handle.is_active());
    }

    /// No store, a revoked store, and a store with no paired device start no
    /// client at app start. A later Begin is a normal first link.
    #[tokio::test]
    async fn a_resume_without_a_paired_store_starts_no_client() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            no_store: true,
            ..Fake::default()
        });
        handle.resume();
        handle.flush().await;
        assert!(fake.log().is_empty(), "no store: {:?}", fake.log());
        assert!(drain(&mut rx).is_empty(), "no event for no store");

        let revoked = Fake::default();
        revoked.revoked.store(true, Ordering::SeqCst);
        let (fake, handle, _session, _rx) = owner(revoked);
        handle.resume();
        handle.flush().await;
        assert!(fake.log().is_empty(), "revoked: {:?}", fake.log());

        let (fake, handle, _session, mut rx) = owner(Fake {
            unpaired: true,
            ..Fake::default()
        });
        handle.resume();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["resume 1"]);
        assert!(!handle.is_active());
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&AdapterEvent::Account {
                protocol: ProtocolId::WhatsApp,
                state: AccountState::Unlinked,
            })
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AdapterEvent::WhatsAppQr { .. })),
            "no QR code at app start"
        );
        handle.begin(None, 9);
        handle.flush().await;
        assert_eq!(fake.log(), vec!["resume 1", "start 2"]);
    }

    /// ADR 0010 rule 8: QR codes and pair codes carry the shell's pairing
    /// id from `WhatsAppBeginLink`, not the owner's internal generation.
    #[tokio::test]
    async fn pairing_payloads_carry_the_shell_pairing_id() {
        let (fake, handle, _session, mut rx) = owner(Fake::default());
        handle.begin(None, 42);
        handle.flush().await;
        fake.callback(1).send(qr("first"));
        fake.callback(1)
            .send(LinkEvent::PairCode(RedactedPairingSecret::new("CODE")));
        handle.flush().await;
        let generations: Vec<u64> = drain(&mut rx)
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::WhatsAppQr { generation, .. }
                | AdapterEvent::WhatsAppPairCode { generation, .. } => Some(*generation),
                _ => None,
            })
            .collect();
        assert_eq!(generations, vec![42, 42]);

        // A new pairing: its payloads carry the new id; the old ones drop.
        handle.begin(None, 43);
        handle.flush().await;
        fake.callback(1).send(qr("old"));
        fake.callback(2).send(qr("new"));
        handle.flush().await;
        let payloads: Vec<(u64, String)> = drain(&mut rx)
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::WhatsAppQr { generation, code } => {
                    Some((*generation, code.reveal().to_string()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(payloads, vec![(43, "new".to_string())]);
    }

    /// Codex r4093899824: a cancel during the start ends it with no late
    /// "pairing is running" status and no late error.
    #[tokio::test]
    async fn cancel_during_start_sends_no_late_status() {
        for start_error in [None, Some(StartError::Build)] {
            let gate = Arc::new(Notify::new());
            let (fake, handle, session, mut rx) = owner(Fake {
                start_gate: Some(Arc::clone(&gate)),
                start_error: start_error.clone(),
                ..Fake::default()
            });
            handle.begin(None, 7);
            while fake.0.callbacks.lock().expect("callbacks").is_empty() {
                tokio::task::yield_now().await;
            }
            // The adapter's cancel: reset the session, then tell the owner.
            let _ = session.reset();
            handle.cancel();
            drain(&mut rx);
            gate.notify_one();
            handle.flush().await;
            let late = drain(&mut rx);
            assert!(late.is_empty(), "late events after cancel: {late:?}");
            let expected: Vec<&str> = if start_error.is_none() {
                vec!["start 1", "stop 1"]
            } else {
                vec!["start 1"]
            };
            assert_eq!(fake.log(), expected);
            assert!(!handle.is_active());
            assert!(!session.is_connected());
        }
    }

    /// r4093309818, r4093606395: a repair right after a logout runs after
    /// the delete, never on the revoked store.
    #[tokio::test]
    async fn logout_then_repair_deletes_before_the_new_start() {
        let (fake, handle, _session, _rx) = owner(Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.begin(None, 7);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec!["start 1", "mark revoked", "stop 1", "delete", "start 3"]
        );
    }

    /// The user unlinks a linked account: the device is logged out, the
    /// client stops, the store is deleted, and the row leaves the linked
    /// state with a normal status line. The logout event that the client
    /// sends for its own log out does nothing more.
    #[tokio::test]
    async fn an_unlink_logs_out_and_deletes_the_store() {
        let (fake, handle, session, mut rx) = owner(Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::Connected);
        handle.flush().await;
        assert!(session.is_connected());
        drain(&mut rx);

        handle.unlink();
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec!["start 1", "log out 1", "mark revoked", "stop 1", "delete"]
        );
        assert!(!handle.is_active());
        assert!(!session.is_connected());
        let events = drain(&mut rx);
        assert!(events.contains(&account(AccountState::Unlinked)));
        assert!(
            events.iter().any(|event| matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Stubbed,
                    detail,
                    ..
                } if detail == UNLINKED
            )),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    ..
                }
            )),
            "an unlink of the user is not an error: {events:?}"
        );

        // The next pairing starts on an empty store, with no second delete.
        handle.begin(None, 8);
        handle.flush().await;
        assert_eq!(fake.log().last().map(String::as_str), Some("start 3"));
    }

    /// With no connection WhatsApp does not confirm the log out. This
    /// computer forgets the device in that case too.
    #[tokio::test]
    async fn an_unlink_with_no_answer_still_deletes_the_store() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            log_out_fails: true,
            ..Fake::default()
        });
        handle.begin(None, 7);
        handle.flush().await;
        handle.unlink();
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec!["start 1", "log out 1", "mark revoked", "stop 1", "delete"]
        );
        assert!(drain(&mut rx).contains(&account(AccountState::Unlinked)));
    }

    /// A saved store with no client (for example after a denied key read)
    /// is deleted too. No client exists, so no log out goes out.
    #[tokio::test]
    async fn an_unlink_with_no_client_deletes_the_saved_store() {
        let (fake, handle, _session, mut rx) = owner(Fake::default());
        handle.unlink();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["mark revoked", "delete"]);
        assert!(drain(&mut rx).contains(&account(AccountState::Unlinked)));
    }

    /// A delete that fails leaves the revoked mark. The next pairing deletes
    /// the store before it starts.
    #[tokio::test]
    async fn an_unlink_whose_delete_fails_deletes_at_the_next_pairing() {
        let (fake, handle, _session, _rx) = owner(Fake {
            delete_failures: AtomicUsize::new(1),
            ..Fake::default()
        });
        handle.begin(None, 7);
        handle.flush().await;
        handle.unlink();
        handle.begin(None, 8);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "log out 1",
                "mark revoked",
                "stop 1",
                "delete failed",
                "delete",
                "start 3"
            ]
        );
    }

    /// r4093192847, r4093309814, r4093726770: callbacks of an old link after
    /// a repair do nothing: no stop, no delete, no session change.
    #[tokio::test]
    async fn late_callbacks_of_an_old_link_are_dropped() {
        let (fake, handle, session, mut rx) = owner(Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        handle.begin(None, 7);
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
        handle.begin(None, 7);
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
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        drain(&mut rx);
        handle.begin(None, 7);
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == STORE_FAILED
        )));
        assert!(!handle.is_active());
        handle.begin(None, 7);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "mark revoked",
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
            handle.begin(None, 7);
            handle.flush().await;
            let events = drain(&mut rx);
            assert!(events.iter().any(|event| matches!(
                event,
                AdapterEvent::Status { status: AdapterStatus::Error, detail: text, .. } if text == detail
            )));
            // A failed start ends the pairing: Linking, then Unlinked.
            let states: Vec<AccountState> = events
                .iter()
                .filter_map(|event| match event {
                    AdapterEvent::Account { state, .. } => Some(*state),
                    _ => None,
                })
                .collect();
            assert_eq!(states, vec![AccountState::Linking, AccountState::Unlinked]);
            assert!(!handle.is_active());
            // A late callback of the failed start does nothing.
            fake.callback(1).send(LinkEvent::Connected);
            handle.flush().await;
            assert!(!session.is_connected());
            assert!(handle.shutdown().await);
            assert_eq!(fake.log(), vec!["start 1"], "nothing to stop");
        }
    }

    /// `device.sqlite` is already on disk. A key denial marks the account
    /// linked and does not end it. Retry reads the key again.
    #[tokio::test]
    async fn a_saved_whatsapp_store_stays_linked_when_the_key_is_denied() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            start_error: Some(StartError::KeychainDenied { saved: true }),
            ..Fake::default()
        });
        handle.begin(Some("15551212".into()), 4);
        handle.flush().await;
        let events = drain(&mut rx);
        let states: Vec<AccountState> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Account { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(states, vec![AccountState::Linking, AccountState::Linked]);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::StoreAccess {
                reason: StoreAccessReason::KeychainDenied,
                ..
            }
        )));
        assert!(
            !fake.log().iter().any(|line| line.contains("delete")),
            "the store stays, log {:?}",
            fake.log()
        );
    }

    /// A denied keychain read keeps the account and does not delete the store.
    /// Retry asks `start` again, which is where the key is read.
    /// A wrong key leaves the account unlinked and still deletes nothing.
    #[tokio::test]
    async fn denied_and_wrong_key_keep_the_store_and_retry_reads_again() {
        for (error, reason, unlinked) in [
            (
                StartError::KeychainDenied { saved: false },
                StoreAccessReason::KeychainDenied,
                false,
            ),
            (StartError::WrongKey, StoreAccessReason::WrongKey, true),
        ] {
            let (fake, handle, _session, mut rx) = owner(Fake {
                start_error: Some(error),
                ..Fake::default()
            });
            handle.begin(Some("15551212".into()), 3);
            handle.flush().await;
            let events = drain(&mut rx);
            assert!(events.iter().any(|event| matches!(
                event,
                AdapterEvent::StoreAccess { reason: seen, .. } if *seen == reason
            )));
            assert_eq!(
                events.iter().any(|event| matches!(
                    event,
                    AdapterEvent::Account {
                        state: AccountState::Unlinked,
                        ..
                    }
                )),
                unlinked
            );
            assert!(
                !fake.log().iter().any(|line| line.contains("delete")),
                "the store stays, log {:?}",
                fake.log()
            );
            handle.retry();
            handle.flush().await;
            assert_eq!(fake.log(), vec!["start 1", "start 2"]);
            let retry = drain(&mut rx);
            assert!(retry.iter().any(|event| matches!(
                event,
                AdapterEvent::StoreAccess { reason: seen, .. } if *seen == reason
            )));
            assert_eq!(
                retry.iter().any(|event| matches!(
                    event,
                    AdapterEvent::Account {
                        state: AccountState::Unlinked,
                        ..
                    }
                )),
                unlinked
            );
        }
    }

    /// The files stayed. The helper does not unlink and does not send a wrong key.
    #[tokio::test]
    async fn a_failed_move_aside_does_not_unlink_or_offer_a_link() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            start_error: Some(StartError::MoveFailed),
            ..Fake::default()
        });
        handle.begin(Some("15551212".into()), 3);
        handle.flush().await;
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Error,
                detail,
                ..
            } if detail == thinwire_protocol::whatsapp_key_wrong_kept()
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::StoreAccess { .. }
                | AdapterEvent::Account {
                    state: AccountState::Unlinked,
                    ..
                }
        )));
        let states: Vec<AccountState> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Account { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(states, vec![AccountState::Linking]);
        assert!(
            !fake.log().iter().any(|line| line.contains("delete")),
            "nothing is deleted, log {:?}",
            fake.log()
        );
        // Retry opens again. `start` is the open, and a wrong key there
        // tries the move. This start still fails the move. Nothing is deleted.
        handle.retry();
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "start 2"]);
        let retry = drain(&mut rx);
        assert!(retry.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Error,
                detail,
                ..
            } if detail == thinwire_protocol::whatsapp_key_wrong_kept()
        )));
        assert!(!retry.iter().any(|event| matches!(
            event,
            AdapterEvent::StoreAccess { .. }
                | AdapterEvent::Account {
                    state: AccountState::Unlinked,
                    ..
                }
        )));
        assert!(
            !fake.log().iter().any(|line| line.contains("delete")),
            "retry deletes nothing, log {:?}",
            fake.log()
        );
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
            handle.begin(None, 7);
            handle.flush().await;
            fake.callback(1).send(event);
            handle.flush().await;
            handle.begin(None, 7);
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
        handle.begin(None, 7); // generation 1
        handle.flush().await;
        let first = fake.callback(1);
        first.send(LinkEvent::LoggedOut); // generation 2
        handle.cancel(); // generation 3
        first.send(LinkEvent::Connected);
        handle.begin(None, 7); // generation 4
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
        assert_eq!(
            fake.log(),
            vec!["start 1", "mark revoked", "stop 1", "delete", "start 4"]
        );
        assert!(!session.is_connected());

        fake.callback(4).send(qr("fresh"));
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::WhatsAppQr { generation: 7, code } if code.reveal() == "fresh"
        )));
    }

    /// Codex r4093899843: a failed delete of a revoked store survives a
    /// restart. The next start after the restart deletes it first.
    #[tokio::test]
    async fn revoked_mark_survives_a_restart() {
        let (fake, handle, _session, _rx) = owner(Fake {
            delete_failures: AtomicUsize::new(1),
            ..Fake::default()
        });
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        drop(handle);

        // A restart: a new owner with no memory, on the same "disk".
        let (tx, _rx) = unbounded_channel();
        let restarted = LinkHandle::spawn(fake.clone(), Session::default(), tx);
        restarted.begin(None, 8);
        restarted.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "mark revoked",
                "stop 1",
                "delete failed",
                "delete",
                "start 1"
            ]
        );
        assert!(
            !fake.0.revoked.load(Ordering::SeqCst),
            "a delete clears the mark"
        );

        // Without a mark, a start does not delete anything.
        restarted.begin(None, 9);
        restarted.flush().await;
        assert_eq!(fake.log().last().map(String::as_str), Some("start 2"));
        assert!(
            !fake
                .log()
                .iter()
                .skip(6)
                .any(|entry| entry.starts_with("delete"))
        );
    }

    /// Codex r4103855622: a failed mark write is not silent. If the delete
    /// also fails, the user gets an error, and a Begin in this run still
    /// deletes the store first.
    #[tokio::test]
    async fn failed_mark_and_failed_delete_report_an_error() {
        let (fake, handle, _session, mut rx) = owner(Fake {
            mark_fails: true,
            delete_failures: AtomicUsize::new(1),
            ..Fake::default()
        });
        handle.begin(None, 7);
        handle.flush().await;
        drain(&mut rx);
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == REVOKE_NOT_SAVED
        )));
        handle.begin(None, 8);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "mark failed",
                "stop 1",
                "delete failed",
                "delete",
                "start 3"
            ]
        );

        // A failed mark with a working delete needs no error.
        let (fake, handle, _session, mut rx) = owner(Fake {
            mark_fails: true,
            ..Fake::default()
        });
        handle.begin(None, 7);
        handle.flush().await;
        drain(&mut rx);
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert!(!drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { detail, .. } if detail == REVOKE_NOT_SAVED
        )));
    }

    /// An owner whose client stops only when `gate` opens. The stop bound is
    /// zero: a gated stop cannot end before the gate opens, so every stop is
    /// slow with no timer race (#167).
    fn slow_stop_owner(
        gate: &Arc<Notify>,
        fake: Fake,
    ) -> (Shared, LinkHandle, Session, UnboundedReceiver<AdapterEvent>) {
        let shared = Shared(Arc::new(Fake {
            stop_gate: Some(Arc::clone(gate)),
            ..fake
        }));
        let (tx, rx) = unbounded_channel();
        let session = Session::default();
        let handle =
            LinkHandle::spawn_with_stop_wait(shared.clone(), session.clone(), tx, Duration::ZERO);
        (shared, handle, session, rx)
    }

    /// #98 (Codex r4104252007): a stop that hangs does not block the owner.
    /// The owner reports it and refuses a new pairing until the old client
    /// stopped; then a pairing starts again.
    #[tokio::test]
    async fn a_slow_stop_is_bounded_and_blocks_the_next_pairing() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, _session, mut rx) = slow_stop_owner(&gate, Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        drain(&mut rx);

        handle.cancel();
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == STOP_PENDING
        )));

        // A new pairing is refused while the old client still stops.
        handle.begin(None, 8);
        handle.flush().await;
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == STILL_RUNNING
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Account {
                state: AccountState::Unlinked,
                ..
            }
        )));
        assert_eq!(fake.log(), vec!["start 1", "stop 1"]);
        assert!(!handle.is_active());

        // The old client stops. Its error goes (#158 review); then a pairing
        // starts.
        gate.notify_one();
        let ended = next_status(&mut rx).await;
        assert_eq!(ended, (AdapterStatus::Stubbed, STOP_ENDED.to_string()));
        assert!(fake.log().iter().any(|entry| entry == "stopped 1"));
        handle.begin(None, 9);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec!["start 1", "stop 1", "stopped 1", "start 4"]
        );
    }

    /// The next status the owner sends. It waits for the event itself, with
    /// no poll (#167).
    async fn next_status(rx: &mut UnboundedReceiver<AdapterEvent>) -> (AdapterStatus, String) {
        loop {
            match rx.recv().await.expect("the owner is alive") {
                AdapterEvent::Status { status, detail, .. } => return (status, detail),
                _ => continue,
            }
        }
    }

    /// #158 review: a logout whose stop ends late gets its reason back, not
    /// the stop error and not a plain "stopped".
    #[tokio::test]
    async fn a_late_stop_end_restores_the_stop_reason() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, session, mut rx) = slow_stop_owner(&gate, Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == STOP_PENDING
        )));
        gate.notify_one();
        let reason = session.stopped().expect("a logout reason");
        assert_eq!(
            next_status(&mut rx).await,
            (AdapterStatus::Error, reason.to_string())
        );
    }

    /// #167: after a logout with a slow stop, the revoked store goes when the
    /// stop ends, before any new pairing.
    #[tokio::test]
    async fn a_revoked_store_is_deleted_when_its_slow_stop_ends() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, _session, mut rx) = slow_stop_owner(&gate, Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        assert_eq!(fake.log(), vec!["start 1", "mark revoked", "stop 1"]);
        drain(&mut rx);

        gate.notify_one();
        next_status(&mut rx).await;
        assert_eq!(
            fake.log(),
            vec!["start 1", "mark revoked", "stop 1", "stopped 1", "delete"]
        );
        assert!(!fake.0.revoked.load(Ordering::SeqCst), "the mark goes too");
    }

    /// #167: when neither the mark nor the delete at the stop end reaches
    /// the disk, the warning stays. The next Begin deletes the store first.
    #[tokio::test]
    async fn a_failed_delete_at_the_stop_end_keeps_the_warning() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, _session, mut rx) = slow_stop_owner(
            &gate,
            Fake {
                mark_fails: true,
                delete_failures: AtomicUsize::new(1),
                ..Fake::default()
            },
        );
        handle.begin(None, 7);
        handle.flush().await;
        fake.callback(1).send(LinkEvent::LoggedOut);
        handle.flush().await;
        drain(&mut rx);

        gate.notify_one();
        assert_eq!(
            next_status(&mut rx).await,
            (AdapterStatus::Error, REVOKE_NOT_SAVED.to_string())
        );
        handle.begin(None, 8);
        handle.flush().await;
        assert_eq!(
            fake.log(),
            vec![
                "start 1",
                "mark failed",
                "stop 1",
                "stopped 1",
                "delete failed",
                "delete",
                "start 3"
            ]
        );
    }

    /// #98: a shutdown whose stop does not end in time gets no confirmation,
    /// so the adapter sends no false `Stopped` (#44).
    #[tokio::test]
    async fn shutdown_with_a_slow_stop_is_not_confirmed() {
        let gate = Arc::new(Notify::new());
        let (_fake, handle, _session, _rx) = slow_stop_owner(&gate, Fake::default());
        handle.begin(None, 7);
        handle.flush().await;
        // The owner itself answers "not stopped": it drops the reply. The
        // long limit is only a guard; no timer decides the result (#167).
        let (done, wait) = oneshot::channel();
        handle
            .tx
            .send(Msg::Shutdown { done })
            .unwrap_or_else(|_| panic!("the owner is alive"));
        let answer = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("the owner answers without a timer");
        assert!(answer.is_err(), "a slow stop is not confirmed");
        assert!(STOP_WAIT < SHUTDOWN_WAIT);
    }

    /// #44: shutdown waits for a pending start, then stops that client.
    #[tokio::test]
    async fn shutdown_waits_for_a_pending_start_then_stops_it() {
        let gate = Arc::new(Notify::new());
        let (fake, handle, _session, _rx) = owner(Fake {
            start_gate: Some(Arc::clone(&gate)),
            ..Fake::default()
        });
        handle.begin(None, 7);
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
        handle.begin(None, 7);
        handle.flush().await;
        let started = std::time::Instant::now();
        assert!(!handle.shutdown_within(Duration::from_millis(50)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(SHUTDOWN_WAIT < Duration::from_secs(5));
    }
}
