//! Live bot inbox session: one owner task per session (#138).
//!
//! A single tokio task ([`Owner`]) owns all session state: the channel map,
//! the bot id, the history ids, the row texts, the sends and loads in flight,
//! the reload ticket, the pending unlink, and the retired and shutdown state.
//! Every change is a [`Msg`] on one channel. The owner handles the messages
//! one at a time, in order. HTTP calls run as spawned tasks. A task only
//! sends its result back as a message: it never reads or writes session
//! state, and it never emits an event. So no lock is held across an await,
//! and there is no check/await/act gap.
//!
//! The adapter keeps a [`Session`] handle. Its methods only send messages. It
//! reads one-way flags: `ready` (the bot id is known), `ending` (a 401
//! settled, `Unlinked` is pending), `revoked` (the owner queued `Unlinked`),
//! and `retired`. The owner sets the first three. [`Session::retire`] and
//! [`Session::disconnect`] set `retired` before they return, so a list or
//! history result already queued publishes nothing.
//!
//! Retirement stops new work only. A send that started before it still gets
//! its real result, also after the owner has handled the retirement: a send
//! that went out ends as `SendAccepted`, while the account is still open.
//! Every send in flight for the account, including one carried from the
//! session before, gets `SendAccepted` or `SendRejected` (and its row)
//! before the `Unlinked` or `Stopped` that ends the account. The wait is
//! [`SEND_SETTLE_WAIT`]; then the rest are rejected. Nothing for that
//! request is published after that end. The account gate is held from the
//! check through the publication, and through the end event.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, oneshot};

use super::api::{DiscordApi, DiscordApiError, MessageSummary};
use super::inbox::{
    HISTORY_LIMIT, InboxChannel, PREVIEW_FETCH_CONCURRENCY, PreviewPause, channel_preview,
    chat_message, load_channels,
};
use super::{BOT_TOKEN_PRESENT, UNKNOWN_CHANNEL_REFUSAL};
use crate::adapter::{
    AccountState, AdapterError, AdapterEvent, AdapterStatus, Arrival, ChatMessage, Conversation,
    Delivery, EventTx, ProtocolId, emit_account, emit_chat_list_loaded, emit_command_failed,
    emit_conversation, emit_conversation_removed, emit_history_loaded, emit_message,
    emit_message_delivery, emit_message_replaced, emit_notice, emit_send_accepted,
    emit_send_rejected, emit_status, emit_stopped,
};

const READ_ONLY_REFUSAL: &str = "The bot does not have Send Messages in that channel.";
const EMPTY_SEND_REFUSAL: &str = "A message needs text.";
const STILL_LOADING: &str = "Discord bot inbox is still loading guild channels.";

/// How long the 401 step waits for the HTTP results of sends in flight. A
/// send that went out in this time ends as `SendAccepted`, so the user does
/// not send it twice (Codex r4109120586). The rest are rejected, then the
/// seal starts.
const SEND_SETTLE_WAIT: Duration = Duration::from_secs(1);

/// Unix seconds on the local clock. A pending row uses this so it sorts after
/// history until Discord's snowflake time replaces it.
fn local_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
struct ChannelAccess {
    channel_id: u64,
    can_send: bool,
}

/// What a session hands to the session that replaces it.
#[derive(Debug)]
pub(crate) struct Carried {
    /// Channel ids from the last list. The next list removes gone rows.
    channels: HashMap<String, ChannelAccess>,
    /// Message ids from the last history page of each channel. Without them
    /// the next page cannot remove rows the shell still shows. Shared with
    /// the previous session, so a send accepted after the handoff is in this
    /// history too.
    history: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// Texts of outgoing rows, so Retry still works after a reconnect.
    /// Shared with the previous session: an accept removes the text here
    /// and there.
    bodies: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for Carried {
    fn default() -> Self {
        Self {
            channels: HashMap::new(),
            history: Arc::new(Mutex::new(HashMap::new())),
            bodies: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Carried {
    /// The state for the next session. History ids and row texts are shared,
    /// not copied: a send that ends here updates the session that took over.
    fn handoff(&mut self) -> Self {
        Self {
            channels: std::mem::take(&mut self.channels),
            history: Arc::clone(&self.history),
            bodies: Arc::clone(&self.bodies),
        }
    }

    fn bodies(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.bodies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn history(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<String>>> {
        self.history.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The row of a send in flight: a new optimistic row, or a retried row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendRow {
    Pending,
    Retry,
}

#[derive(Debug, Clone)]
struct Inflight {
    conversation_id: String,
    message_id: String,
    row: SendRow,
    bot_id: u64,
}

/// Every send still in flight for the account, including one a previous
/// session started. A result and the `Unlinked` or `Stopped` that ends the
/// account both hold this lock, so the result cannot follow that end.
pub(crate) struct AccountSends {
    /// False after `Unlinked` or `Stopped`. A later result publishes nothing.
    open: bool,
    /// Bumped when a session opens the account, and when shutdown stops it.
    /// A seal publishes only for the epoch that armed it.
    epoch: u64,
    inflight: HashMap<u64, Inflight>,
}

pub(crate) type SharedSends = Arc<Mutex<AccountSends>>;

pub(crate) fn shared_sends() -> SharedSends {
    Arc::new(Mutex::new(AccountSends {
        open: true,
        epoch: 0,
        inflight: HashMap::new(),
    }))
}

/// An idle account can end now. [`IdleEnd::Busy`] means a send is still open.
pub(crate) enum IdleEnd {
    /// This call closed the account. The caller emits `Unlinked`.
    Ended,
    /// The account is already closed.
    Already,
    /// A send is still in flight. The owner answers it, then emits `Unlinked`.
    Busy,
}

impl AccountSends {
    /// Closes the account when nothing is in flight.
    pub(crate) fn take_idle(&mut self) -> IdleEnd {
        if !self.inflight.is_empty() {
            return IdleEnd::Busy;
        }
        if !self.open {
            return IdleEnd::Already;
        }
        self.open = false;
        IdleEnd::Ended
    }

    /// Answers every open send, then emits `Stopped`. The epoch moves, so a
    /// retiring owner seals nothing after this.
    pub(crate) fn stop(&mut self, events: &EventTx) {
        let mut sends: Vec<(u64, Inflight)> = self.inflight.drain().collect();
        sends.sort_by_key(|(request, _)| *request);
        self.open = false;
        self.epoch = self.epoch.saturating_add(1);
        for (request, tracked) in &sends {
            settle_row(events, tracked);
            emit_send_rejected(
                events,
                ProtocolId::Discord,
                &tracked.conversation_id,
                *request,
            );
        }
        emit_stopped(events, ProtocolId::Discord);
    }
}

/// How an account ends. The sends still in flight are answered first.
enum AccountEnd {
    Unlinked,
    Stopped,
    Disconnect { detail: &'static str, attempt: u64 },
}

enum Outgoing {
    New(String),
    Retry(String),
}

enum Msg {
    Reload,
    Open {
        conversation_id: String,
    },
    Send {
        conversation_id: String,
        request: u64,
        outgoing: Outgoing,
    },
    View {
        conversation_id: Option<String>,
    },
    ReloadDone {
        ticket: u64,
        result: Result<(u64, Vec<InboxChannel>), DiscordApiError>,
    },
    /// A preview task asks whether its channel is still in the current list,
    /// before it spends an HTTP call.
    PreviewCheck {
        ticket: u64,
        conversation_id: String,
        listed: oneshot::Sender<bool>,
    },
    PreviewDone {
        ticket: u64,
        conversation: Conversation,
    },
    /// A preview history call returned 401. The owner unlinks with the same
    /// step as history, while this list is still current.
    PreviewUnauthorized {
        ticket: u64,
    },
    HistoryDone {
        load: u64,
        result: Result<Vec<MessageSummary>, DiscordApiError>,
    },
    SendDone {
        request: u64,
        result: Result<MessageSummary, DiscordApiError>,
    },
    /// [`SEND_SETTLE_WAIT`] of a disconnect ended.
    DisconnectWaitOver,
    /// [`SEND_SETTLE_WAIT`] of the 401 step `unlink` ended.
    SendWaitOver {
        unlink: u64,
    },
    /// The seal of the 401 step `unlink` ended: queue `Unlinked` now.
    SealDone {
        unlink: u64,
    },
    /// [`SEND_SETTLE_WAIT`] ended for sends carried into an account end.
    AccountSettle,
    Retire {
        carried: oneshot::Sender<Carried>,
        /// The generation of the session that replaces this one. `None` when
        /// nothing replaces it: the account ends after its sends are answered.
        successor: Option<u64>,
    },
    /// The user disconnected: settle the work in flight, then emit the
    /// disconnected state and publish nothing more.
    ///
    /// `attempt` is the generation [`Session::disconnect`] stored in
    /// [`ActiveSession`]. The owner emits only while that generation is still
    /// the newest, so a later connect or disconnect wins.
    Disconnect {
        detail: &'static str,
        attempt: u64,
    },
    Shutdown {
        done: oneshot::Sender<()>,
        limit: Duration,
    },
    /// The shutdown limit ended with sends still in flight.
    ShutdownLimit,
    #[cfg(test)]
    Flush {
        done: oneshot::Sender<()>,
    },
    /// Test hook: the owner waits before its next message.
    #[cfg(test)]
    Park {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
}

/// The one-way flags that the adapter reads.
#[derive(Debug, Default)]
struct Flags {
    ready: AtomicBool,
    ending: AtomicBool,
    revoked: AtomicBool,
    /// `Stopped` is queued. Whoever sets it first emits it, so it comes once.
    stopped: AtomicBool,
    /// Set by [`Session::retire`] and [`Session::disconnect`] before they
    /// return. A list or history result already queued still sees it and
    /// publishes nothing, so disconnect can emit `Unlinked` without a later
    /// `Linked`, conversation, or history row (Codex r4131954091). Send
    /// results do not use it: the owner applies every send result until the
    /// account gate closes. The close answers each send still in flight, then
    /// emits `Unlinked` or `Stopped` (Codex r4132922551, #165).
    retired: AtomicBool,
}

impl Flags {
    fn emit_stopped_once(&self, events: &EventTx) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            emit_stopped(events, ProtocolId::Discord);
        }
    }
}

/// The id of the newest connect or disconnect attempt. A session start sets
/// it and emits `Linking` under this lock. Every connect and disconnect sets
/// it too, including a connect that starts no session. An owner emits its
/// disconnected state under the lock, and only while its attempt is still
/// the newest. So an old disconnect cannot follow a newer `Linking` or
/// overwrite a missing-token or refused status (Codex r4132725575,
/// r4132922561). The lock is held only for that check and emit, never across
/// an await.
pub(crate) type ActiveSession = Arc<Mutex<u64>>;

/// The adapter side of a session. Every method only sends a message.
pub(crate) struct Session {
    tx: mpsc::UnboundedSender<Msg>,
    flags: Arc<Flags>,
}

impl Session {
    /// Starts a session and loads the channel list.
    ///
    /// `id` makes pending row ids unique across sessions. `handoff` is the
    /// state of the session that this one replaces. The owner waits for it
    /// before its first message, so the old owner has handled all of its
    /// earlier messages first.
    pub(crate) fn start(
        api: Arc<dyn DiscordApi>,
        events: &EventTx,
        id: u64,
        handoff: Option<oneshot::Receiver<Carried>>,
        active: &ActiveSession,
        sends: &SharedSends,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let flags = Arc::new(Flags::default());
        // The epoch bump and `Linking` share the gate lock, so an older seal
        // cannot emit `Unlinked` after this link.
        let epoch = {
            let mut gate = sends.lock().unwrap_or_else(PoisonError::into_inner);
            gate.epoch = gate.epoch.saturating_add(1);
            gate.open = true;
            let mut newest = active.lock().unwrap_or_else(PoisonError::into_inner);
            *newest = id;
            emit_account(events, ProtocolId::Discord, AccountState::Linking);
            emit_status(
                events,
                ProtocolId::Discord,
                AdapterStatus::Connecting,
                format!("Discord bot inbox is loading guild channels. {BOT_TOKEN_PRESENT}."),
            );
            gate.epoch
        };
        let owner = Owner {
            api,
            events: events.clone(),
            tx: tx.downgrade(),
            tx_hold: Some(tx.clone()),
            flags: Arc::clone(&flags),
            active: Arc::clone(active),
            account: Arc::clone(sends),
            epoch,
            id,
            carried: Carried::default(),
            bot_id: None,
            inflight: HashMap::new(),
            loads: HashMap::new(),
            next_load: 0,
            next_pending: 0,
            reload_ticket: 0,
            unlink: None,
            next_unlink: 0,
            retired: false,
            disconnecting: None,
            closed: false,
            send_tasks: 0,
            shutdown: None,
            pending_end: None,
            settle_armed: false,
        };
        let _ = tx.send(Msg::Reload);
        tokio::spawn(owner.run(rx, handoff));
        Self { tx, flags }
    }

    /// True after the owner queued `Unlinked`. The adapter then drops the
    /// session on the next command that is not a send.
    pub(crate) fn is_revoked(&self) -> bool {
        self.flags.revoked.load(Ordering::SeqCst)
    }

    /// True while a 401 is settled and `Unlinked` is still pending.
    pub(crate) fn is_ending(&self) -> bool {
        self.flags.ending.load(Ordering::SeqCst)
    }

    /// Loads the guild channel list again. Channels that left the list are removed.
    pub(crate) fn reload(&self) {
        let _ = self.tx.send(Msg::Reload);
    }

    /// Loads recent messages for a listed channel, oldest first.
    pub(crate) fn open(&self, conversation_id: String) -> Result<(), AdapterError> {
        if !self.is_ending() {
            self.check_ready()?;
        }
        let _ = self.tx.send(Msg::Open { conversation_id });
        Ok(())
    }

    /// Sends as the bot. A pending row shows at once and is replaced on success.
    ///
    /// `request` is the UI's send id. Acceptance is the HTTP success, not the
    /// optimistic row. A failure names that id in `SendRejected` so the draft stays.
    pub(crate) fn send(
        &self,
        conversation_id: String,
        body: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        self.send_outgoing(conversation_id, request, Outgoing::New(body), events)
    }

    /// Posts a failed outgoing row again. The shell names that row and a new request.
    pub(crate) fn resend(
        &self,
        conversation_id: String,
        message_id: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        self.send_outgoing(
            conversation_id,
            request,
            Outgoing::Retry(message_id),
            events,
        )
    }

    fn send_outgoing(
        &self,
        conversation_id: String,
        request: u64,
        outgoing: Outgoing,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        // An ending or revoked session still answers a send (`SendRejected`).
        if !self.is_ending() && !self.is_revoked() {
            self.check_ready()?;
        }
        let sent = self.tx.send(Msg::Send {
            conversation_id,
            request,
            outgoing,
        });
        // The owner is gone, so no row was emitted: answer here, so the send
        // does not stay open (Codex r4130981018).
        if let Err(mpsc::error::SendError(Msg::Send {
            conversation_id,
            request,
            ..
        })) = sent
        {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
        }
        Ok(())
    }

    /// The user looks at this chat. While `Unlinked` is pending, the owner
    /// answers with `CommandFailed`.
    pub(crate) fn view(&self, conversation_id: Option<String>) {
        let _ = self.tx.send(Msg::View { conversation_id });
    }

    fn check_ready(&self) -> Result<(), AdapterError> {
        if self.flags.ready.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(AdapterError::Unavailable {
                protocol: ProtocolId::Discord,
                reason: STILL_LOADING,
            })
        }
    }

    /// A later session replaces this one. List and history results that are
    /// still running are dropped. A send still in flight gets its real
    /// result when its HTTP call ends. The receiver gets the state for the
    /// next session.
    ///
    /// Retirement is visible before this returns. A [`Msg::ReloadDone`] or
    /// [`Msg::HistoryDone`] already queued publishes nothing, even though the
    /// owner handles that message before [`Msg::Retire`]. A send result, queued
    /// or later, is still applied: it may emit `SendAccepted` and
    /// `MessageReplaced` after the new session's `Linking` (keyed by request
    /// and message id), while the account stays open. `successor` is that
    /// generation. `None` ends the account: each send still in flight is
    /// answered, then `Unlinked` follows.
    pub(crate) fn retire(self, successor: Option<u64>) -> oneshot::Receiver<Carried> {
        self.flags.retired.store(true, Ordering::SeqCst);
        let (carried, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Retire { carried, successor });
        rx
    }

    /// The user disconnects. The owner ends the loads at once. Sends in
    /// flight get up to [`SEND_SETTLE_WAIT`] for their real result; the rest
    /// are rejected. Then the owner emits `detail` as a stubbed status and
    /// `Unlinked`, and publishes nothing more. So no event of this session
    /// follows the disconnected state (Codex r4131954091). When the owner is
    /// gone, the handle emits that state itself.
    ///
    /// `attempt` is the generation just stored in [`ActiveSession`]. The
    /// owner emits only while that generation is still the newest.
    pub(crate) fn disconnect(self, detail: &'static str, attempt: u64, events: &EventTx) {
        self.flags.retired.store(true, Ordering::SeqCst);
        if self.tx.send(Msg::Disconnect { detail, attempt }).is_err() {
            emit_disconnected(events, detail);
        }
    }

    /// The app closes. The owner waits until every send in flight has its
    /// result, at most `limit`. Then it closes: it emits `Stopped` once and
    /// publishes nothing more (Codex r4130981025). The returned value waits
    /// for that, and emits `Stopped` itself only if the owner is gone.
    pub(crate) fn shutdown(self, limit: Duration) -> Closing {
        let (done, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Shutdown { done, limit });
        Closing {
            done: rx,
            flags: self.flags,
            limit,
        }
    }

    /// Test hook: completes after the owner handled every earlier message.
    #[cfg(test)]
    pub(crate) async fn flush(&self) {
        let (done, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush { done }).is_ok() {
            let _ = rx.await;
        }
    }

    /// Test hook: the owner waits before its next message. Drop the guard,
    /// or call [`Parked::release`], to let it continue.
    #[cfg(test)]
    pub(crate) async fn park(&self) -> Parked {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        self.tx
            .send(Msg::Park {
                entered,
                release: release_rx,
            })
            .expect("owner alive");
        entered_rx.await.expect("owner parked");
        Parked {
            release: Some(release),
        }
    }
}

/// Releases a [`Session::park`] so the owner reads its next message.
#[cfg(test)]
pub(crate) struct Parked {
    release: Option<oneshot::Sender<()>>,
}

#[cfg(test)]
impl Parked {
    pub(crate) fn release(mut self) {
        self.signal();
    }

    fn signal(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

#[cfg(test)]
impl Drop for Parked {
    fn drop(&mut self) {
        self.signal();
    }
}

/// Extra time over the shutdown limit before the adapter stops waiting for
/// a stuck owner. The owner's own timer at the limit is the real bound.
pub(crate) const CLOSING_MARGIN: Duration = Duration::from_millis(500);

/// The end of a shutdown. See [`Session::shutdown`].
pub(crate) struct Closing {
    done: oneshot::Receiver<()>,
    flags: Arc<Flags>,
    limit: Duration,
}

impl Closing {
    pub(crate) async fn wait(self, events: &EventTx) {
        let closed = tokio::time::timeout(self.limit + CLOSING_MARGIN, self.done).await;
        if !matches!(closed, Ok(Ok(()))) {
            // The owner is gone or stuck. `Stopped` still comes once.
            self.flags.emit_stopped_once(events);
        }
    }
}

/// A 401 that settled the loads. `Unlinked` waits for the sends and the seal.
struct PendingUnlink {
    id: u64,
    error: DiscordApiError,
    /// The channel list failed: an error status follows `Unlinked`.
    from_reload: bool,
    /// The sends are settled and the seal task runs.
    sealing: bool,
}

struct Owner {
    api: Arc<dyn DiscordApi>,
    events: EventTx,
    /// Weak: the owner ends when the session handle and every task that
    /// reports back are gone. A task holds a strong sender.
    tx: mpsc::WeakUnboundedSender<Msg>,
    /// Strong sender for a timer armed after the session handle is gone.
    /// A carried send's HTTP task holds the previous owner's sender, not
    /// this one. Dropped once the account has ended, or once this session
    /// has handed off, so the task can finish.
    tx_hold: Option<mpsc::UnboundedSender<Msg>>,
    flags: Arc<Flags>,
    /// See [`ActiveSession`].
    active: ActiveSession,
    /// See [`AccountSends`]. Shared with every session of this account.
    account: SharedSends,
    /// The gate epoch this session opened. A seal for an older epoch publishes
    /// nothing.
    epoch: u64,
    id: u64,
    carried: Carried,
    bot_id: Option<u64>,
    /// Sends in flight, by request. Each one ends with exactly one
    /// `SendAccepted` or `SendRejected`.
    inflight: HashMap<u64, Inflight>,
    /// History loads in flight, by load id, with their chat.
    loads: HashMap<u64, String>,
    next_load: u64,
    next_pending: u64,
    /// The latest reload. An older list or failure publishes nothing.
    reload_ticket: u64,
    unlink: Option<PendingUnlink>,
    next_unlink: u64,
    /// A later session replaced this one, or the app closed.
    retired: bool,
    /// The user disconnected. The owner waits for its sends in flight, then
    /// emits the disconnected state with this detail, for this attempt.
    disconnecting: Option<(&'static str, u64)>,
    /// The app closed: `Stopped` is queued and nothing more is published.
    closed: bool,
    /// HTTP send tasks still running. Shutdown waits for zero.
    send_tasks: u64,
    shutdown: Option<oneshot::Sender<()>>,
    /// An account end is waiting out [`SEND_SETTLE_WAIT`].
    pending_end: Option<AccountEnd>,
    /// The settle timer is already armed.
    settle_armed: bool,
}

impl Owner {
    async fn run(
        mut self,
        mut rx: mpsc::UnboundedReceiver<Msg>,
        handoff: Option<oneshot::Receiver<Carried>>,
    ) {
        if let Some(handoff) = handoff {
            self.carried = handoff.await.unwrap_or_default();
        }
        while let Some(msg) = rx.recv().await {
            #[cfg(test)]
            if let Msg::Park { entered, release } = msg {
                let _ = entered.send(());
                let _ = release.await;
                continue;
            }
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        if self.closed {
            // After `Stopped`, nothing more is published.
            #[cfg(test)]
            if let Msg::Flush { done } = msg {
                let _ = done.send(());
            }
            return;
        }
        match msg {
            Msg::Reload => self.reload(),
            Msg::Open { conversation_id } => self.open(conversation_id),
            Msg::Send {
                conversation_id,
                request,
                outgoing,
            } => self.send(conversation_id, request, outgoing),
            Msg::View { conversation_id } => {
                if let Some(error) = self.ending_error() {
                    emit_command_failed(
                        &self.events,
                        ProtocolId::Discord,
                        conversation_id,
                        error.reason(),
                    );
                }
            }
            Msg::ReloadDone { ticket, result } => self.reload_done(ticket, result),
            Msg::PreviewCheck {
                ticket,
                conversation_id,
                listed,
            } => {
                let _ = listed.send(self.still_listed(ticket, &conversation_id));
            }
            Msg::PreviewDone {
                ticket,
                conversation,
            } => {
                if self.still_listed(ticket, &conversation.id) {
                    emit_conversation(&self.events, conversation);
                }
            }
            Msg::PreviewUnauthorized { ticket } => {
                // Same unlink step as history. An older list, or a session
                // that was replaced or is already ending, publishes nothing.
                if ticket == self.reload_ticket && self.publishes() {
                    self.settle_unauthorized(DiscordApiError::Unauthorized, false);
                }
            }
            Msg::HistoryDone { load, result } => self.history_done(load, result),
            Msg::SendDone { request, result } => self.send_done(request, result),
            Msg::SendWaitOver { unlink } => self.send_wait_over(unlink),
            Msg::SealDone { unlink } => self.seal_done(unlink),
            Msg::Retire { carried, successor } => {
                self.retire();
                // A send still in flight shares its text with the next
                // session: if it fails, its row can be retried there. An
                // accept removes that text from the shared map.
                let _ = carried.send(self.carried.handoff());
                if successor.is_none() {
                    // Nothing replaces this session. Answer the sends still
                    // in flight, then emit `Unlinked`. A `SendDone` already
                    // queued is handled first, so it is accepted before that.
                    self.begin_account_end(AccountEnd::Unlinked);
                }
                // The HTTP task, if any, holds its own sender. This one would
                // keep the owner alive after the handoff.
                self.tx_hold = None;
            }
            Msg::AccountSettle => self.account_settle(),
            Msg::Shutdown { done, limit } => {
                self.shutdown = Some(done);
                // `spawn` needs a live strong sender. The session handle is
                // gone now, but while a send is open its task holds one, so
                // the timer starts. With no send open, the owner closes below
                // at once and needs no timer.
                self.spawn(async move {
                    tokio::time::sleep(limit).await;
                    Msg::ShutdownLimit
                });
                self.finish_shutdown_if_idle();
            }
            Msg::ShutdownLimit => self.close(),
            Msg::Disconnect { detail, attempt } => self.disconnect(detail, attempt),
            Msg::DisconnectWaitOver => self.finish_disconnect(),
            #[cfg(test)]
            Msg::Flush { done } => {
                let _ = done.send(());
            }
            // `run` waits on `Park` before it calls `handle`.
            #[cfg(test)]
            Msg::Park { entered, .. } => {
                let _ = entered.send(());
            }
        }
    }

    /// The error of a 401 whose `Unlinked` is still pending.
    fn ending_error(&self) -> Option<DiscordApiError> {
        self.unlink.as_ref().map(|unlink| unlink.error)
    }

    /// The owner queued `Unlinked` for this session.
    fn unlinked(&self) -> bool {
        self.flags.revoked.load(Ordering::SeqCst)
    }

    /// [`Session::retire`] or [`Session::disconnect`] has returned, or this
    /// task has handled `Retire`, `Disconnect`, or the end of a shutdown.
    /// Inbox publications use this. A send result uses [`Self::retired`]
    /// alone when the call succeeded, so one queued before the retirement
    /// message is still accepted.
    fn replaced(&self) -> bool {
        self.retired || self.flags.retired.load(Ordering::SeqCst)
    }

    /// This session may publish inbox rows: not replaced, not ending, and
    /// not unlinked.
    fn publishes(&self) -> bool {
        !self.replaced() && self.unlink.is_none() && !self.unlinked()
    }

    fn still_listed(&self, ticket: u64, conversation_id: &str) -> bool {
        self.publishes()
            && ticket == self.reload_ticket
            && self.carried.channels.contains_key(conversation_id)
    }

    fn retire(&mut self) {
        self.retired = true;
        self.flags.retired.store(true, Ordering::SeqCst);
        // A reconnect replaces this account. Its pending `Unlinked` must not
        // unlink the new session.
        self.unlink = None;
        self.flags.ending.store(false, Ordering::SeqCst);
    }

    fn reload(&mut self) {
        if self.replaced() {
            return;
        }
        if let Some(error) = self.ending_error() {
            emit_command_failed(&self.events, ProtocolId::Discord, None, error.reason());
            return;
        }
        if self.unlinked() {
            return;
        }
        self.reload_ticket = self.reload_ticket.wrapping_add(1);
        let ticket = self.reload_ticket;
        let api = Arc::clone(&self.api);
        self.spawn(async move {
            let result = load_channels(api.as_ref()).await;
            Msg::ReloadDone { ticket, result }
        });
    }

    fn open(&mut self, conversation_id: String) {
        if self.replaced() {
            return;
        }
        if let Some(error) = self.ending_error() {
            // The session is ending. `Unlinked` follows.
            emit_command_failed(
                &self.events,
                ProtocolId::Discord,
                Some(conversation_id),
                error.reason(),
            );
            return;
        }
        if self.unlinked() {
            return;
        }
        let Some(access) = self.carried.channels.get(&conversation_id).copied() else {
            emit_notice(&self.events, ProtocolId::Discord, UNKNOWN_CHANNEL_REFUSAL);
            emit_command_failed(
                &self.events,
                ProtocolId::Discord,
                Some(conversation_id.clone()),
                UNKNOWN_CHANNEL_REFUSAL,
            );
            emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
            return;
        };
        if self.bot_id.is_none() {
            emit_command_failed(
                &self.events,
                ProtocolId::Discord,
                Some(conversation_id.clone()),
                STILL_LOADING,
            );
            emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
            return;
        }
        self.next_load += 1;
        let load = self.next_load;
        self.loads.insert(load, conversation_id);
        let api = Arc::clone(&self.api);
        self.spawn(async move {
            let result = api.history(access.channel_id, HISTORY_LIMIT).await;
            Msg::HistoryDone { load, result }
        });
    }

    fn send(&mut self, conversation_id: String, request: u64, outgoing: Outgoing) {
        let reject_conversation = conversation_id.clone();
        let reject = |owner: &Self| {
            emit_send_rejected(
                &owner.events,
                ProtocolId::Discord,
                &reject_conversation,
                request,
            );
        };
        if let Outgoing::New(body) = &outgoing
            && body.trim().is_empty()
        {
            reject(self);
            emit_notice(&self.events, ProtocolId::Discord, EMPTY_SEND_REFUSAL);
            return;
        }
        if self.replaced() || self.unlink.is_some() || self.unlinked() {
            // An ending or unlinked session registers no send. A pending
            // `Unlinked` comes after this rejection.
            reject(self);
            return;
        }
        let Some(access) = self.carried.channels.get(&conversation_id).copied() else {
            emit_notice(&self.events, ProtocolId::Discord, UNKNOWN_CHANNEL_REFUSAL);
            reject(self);
            return;
        };
        let Some(bot_id) = self.bot_id else {
            reject(self);
            return;
        };
        if !access.can_send {
            reject(self);
            emit_notice(&self.events, ProtocolId::Discord, READ_ONLY_REFUSAL);
            return;
        }
        // The session handle is gone (a disconnect, reconnect, or shutdown
        // follows): no call can report back. Reject before any row shows, so
        // no optimistic row stays pending (Codex r4130981018).
        let Some(tx) = self.tx.upgrade().or_else(|| self.tx_hold.clone()) else {
            reject(self);
            return;
        };
        // A test closes the gate in this gap. The flag check above is already
        // done; registration below re-checks the gate.
        if let Some(pause) = self.api.register_pause() {
            pause.arrived.notify_one();
            tokio::task::block_in_place(|| pause.wait());
        }
        let Some(body) = self.register_send(conversation_id, request, outgoing, bot_id) else {
            reject(self);
            return;
        };
        self.send_tasks += 1;
        let api = Arc::clone(&self.api);
        tokio::spawn(async move {
            let result = api.send(access.channel_id, body).await;
            let _ = tx.send(Msg::SendDone { request, result });
            // The result is with the owner now. A test can run a 401 here.
            if let Some(pause) = api.send_result_pause() {
                pause.arrived.notify_one();
                pause.release.notified().await;
            }
        });
    }

    /// Checks the gate, inserts the send, and emits a new pending row, in one
    /// lock hold. Returns the body to post. `None` rejects with no row: the
    /// account is closed, or this session's epoch has ended.
    fn register_send(
        &mut self,
        conversation_id: String,
        request: u64,
        outgoing: Outgoing,
        bot_id: u64,
    ) -> Option<String> {
        let events = self.events.clone();
        let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        if !gate.open || gate.epoch != self.epoch {
            return None;
        }
        let (body, message_id, row) = match outgoing {
            Outgoing::New(body) => {
                self.next_pending += 1;
                let message_id = format!("discord:pending:{}:{}", self.id, self.next_pending);
                self.carried
                    .bodies()
                    .insert(message_id.clone(), body.clone());
                emit_message(
                    &events,
                    ChatMessage {
                        protocol: ProtocolId::Discord,
                        conversation_id: conversation_id.clone(),
                        id: message_id.clone(),
                        sender: "bot".into(),
                        body: body.clone(),
                        outbound: true,
                        delivery: Delivery::Pending,
                        sent_at: local_unix_seconds(),
                        arrival: Arrival::History,
                    },
                );
                (body, message_id, SendRow::Pending)
            }
            Outgoing::Retry(message_id) => {
                let body = self
                    .carried
                    .bodies()
                    .get(&message_id)
                    .cloned()
                    .filter(|text| !text.trim().is_empty())?;
                (body, message_id, SendRow::Retry)
            }
        };
        let tracked = Inflight {
            conversation_id,
            message_id,
            row,
            bot_id,
        };
        gate.inflight.insert(request, tracked.clone());
        drop(gate);
        self.inflight.insert(request, tracked);
        Some(body)
    }

    fn reload_done(
        &mut self,
        ticket: u64,
        result: Result<(u64, Vec<InboxChannel>), DiscordApiError>,
    ) {
        // An older list or failure, a replaced session, or an ending or
        // unlinked one publishes nothing. It does not unlink the session
        // that replaced it.
        if ticket != self.reload_ticket || !self.publishes() {
            return;
        }
        match result {
            Ok((bot_id, channels)) => {
                self.publish_channels(bot_id, &channels);
                self.spawn_previews(ticket, channels);
            }
            Err(DiscordApiError::Unauthorized) => {
                tracing::info!("discord channel list failed: unauthorized");
                self.settle_unauthorized(DiscordApiError::Unauthorized, true);
            }
            Err(error) => {
                tracing::info!(%error, "discord channel list failed");
                let detail = format!("Discord bot inbox did not load: {error}");
                emit_command_failed(&self.events, ProtocolId::Discord, None, detail.clone());
                emit_status(
                    &self.events,
                    ProtocolId::Discord,
                    AdapterStatus::Error,
                    detail,
                );
            }
        }
    }

    fn publish_channels(&mut self, bot_id: u64, channels: &[InboxChannel]) {
        let next: HashMap<String, ChannelAccess> = channels
            .iter()
            .map(|channel| {
                (
                    channel.conversation_id(),
                    ChannelAccess {
                        channel_id: channel.channel_id,
                        can_send: channel.can_send,
                    },
                )
            })
            .collect();
        let gone: Vec<String> = self
            .carried
            .channels
            .keys()
            .filter(|id| !next.contains_key(*id))
            .cloned()
            .collect();
        self.bot_id = Some(bot_id);
        self.carried.channels = next;
        self.flags.ready.store(true, Ordering::SeqCst);
        emit_account(&self.events, ProtocolId::Discord, AccountState::Linked);
        // Ready before the rows. The shell opens a chat only once the account is linked.
        emit_ready(
            &self.events,
            &format!("{} guild channels the bot can read.", channels.len()),
        );
        for id in gone {
            emit_conversation_removed(&self.events, ProtocolId::Discord, id);
        }
        for channel in channels {
            emit_conversation(&self.events, channel.conversation());
        }
        // The shell stops the chat-list spinner on this event
        // (adapter contract rule 9, ADR 0010).
        emit_chat_list_loaded(&self.events, ProtocolId::Discord);
    }

    /// Fills previews after the account is linked. At most
    /// [`PREVIEW_FETCH_CONCURRENCY`] history calls run at once. A preview
    /// task asks the owner before its call, and the owner publishes its
    /// result only while the channel is still in the current list. A
    /// per-channel failure leaves the empty preview. A 401 uses the same
    /// unlink step as history.
    fn spawn_previews(&self, ticket: u64, channels: Vec<InboxChannel>) {
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        let api = Arc::clone(&self.api);
        tokio::spawn(async move {
            // The list is already on the channel. Yield so the frontend gets
            // it before any preview history call starts.
            tokio::task::yield_now().await;
            start_previews(&api, &tx, ticket, channels);
        });
    }
}

/// One task per channel preview, at most [`PREVIEW_FETCH_CONCURRENCY`] calls
/// at once. Each task asks the owner before its call.
fn start_previews(
    api: &Arc<dyn DiscordApi>,
    tx: &mpsc::UnboundedSender<Msg>,
    ticket: u64,
    channels: Vec<InboxChannel>,
) {
    let pause = Arc::new(PreviewPause::new());
    let limit = Arc::new(Semaphore::new(PREVIEW_FETCH_CONCURRENCY));
    for mut channel in channels {
        let tx = tx.clone();
        let api = Arc::clone(api);
        let pause = Arc::clone(&pause);
        let limit = Arc::clone(&limit);
        tokio::spawn(async move {
            let Ok(_permit) = limit.acquire_owned().await else {
                return;
            };
            let (listed, reply) = oneshot::channel();
            let asked = tx.send(Msg::PreviewCheck {
                ticket,
                conversation_id: channel.conversation_id(),
                listed,
            });
            if asked.is_err() || !reply.await.unwrap_or(false) {
                return;
            }
            let preview = match channel_preview(api.as_ref(), channel.channel_id, &pause).await {
                Ok(preview) => preview,
                Err(DiscordApiError::Unauthorized) => {
                    // The owner unlinks only while this list is still current.
                    let _ = tx.send(Msg::PreviewUnauthorized { ticket });
                    return;
                }
                Err(_) => return,
            };
            if preview.is_empty() {
                return;
            }
            channel.preview = preview;
            let _ = tx.send(Msg::PreviewDone {
                ticket,
                conversation: channel.conversation(),
            });
        });
    }
}

impl Owner {
    fn history_done(&mut self, load: u64, result: Result<Vec<MessageSummary>, DiscordApiError>) {
        // A 401 already finished this load.
        let Some(conversation_id) = self.loads.remove(&load) else {
            return;
        };
        if self.replaced() || self.unlinked() {
            // A newer session owns the account, or this one already ended.
            // `HistoryLoaded` here clears the new session's open-on-link
            // mark, so its `Linked` does not queue `OpenChat` (Codex
            // r4133411242). The new session ends its own load.
            return;
        }
        match result {
            Ok(messages) => {
                let bot_id = self.bot_id.unwrap_or_default();
                let rows: Vec<ChatMessage> = messages
                    .iter()
                    .rev()
                    .map(|message| chat_message(&conversation_id, bot_id, message))
                    .collect();
                let new_ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
                let gone = {
                    let mut history = self.carried.history();
                    let previous = history.entry(conversation_id.clone()).or_default();
                    let gone: Vec<String> = previous
                        .iter()
                        .filter(|id| !new_ids.iter().any(|next| next == *id))
                        .cloned()
                        .collect();
                    *previous = new_ids;
                    gone
                };
                if !gone.is_empty() {
                    let _ = self.events.send(AdapterEvent::MessagesRemoved {
                        protocol: ProtocolId::Discord,
                        conversation_id: conversation_id.clone(),
                        message_ids: gone,
                    });
                }
                for row in rows {
                    emit_message(&self.events, row);
                }
                emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
            }
            Err(DiscordApiError::Unauthorized) => {
                tracing::info!("discord history failed: unauthorized");
                // Put the load back, so the 401 step finishes it with the rest.
                self.loads.insert(load, conversation_id);
                self.settle_unauthorized(DiscordApiError::Unauthorized, false);
            }
            Err(error) => {
                tracing::info!(%error, "discord history failed");
                emit_command_failed(
                    &self.events,
                    ProtocolId::Discord,
                    Some(conversation_id.clone()),
                    format!("History did not load: {error}."),
                );
                emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
            }
        }
    }

    fn send_done(&mut self, request: u64, result: Result<MessageSummary, DiscordApiError>) {
        self.send_tasks = self.send_tasks.saturating_sub(1);
        self.answer_send(request, result);
        // The 401 step waited for this send. The last one starts the seal.
        if !self.account_pending()
            && let Some(unlink) = &self.unlink
            && !unlink.sealing
        {
            let id = unlink.id;
            self.start_seal(id);
        }
        if self.disconnecting.is_some() && !self.account_pending() {
            self.finish_disconnect();
        }
        self.finish_shutdown_if_idle();
    }

    fn answer_send(&mut self, request: u64, result: Result<MessageSummary, DiscordApiError>) {
        // The 401 step or an earlier result already answered this request.
        let Some(tracked) = self.inflight.remove(&request) else {
            return;
        };
        // The account gate stays held through the publication. A concurrent
        // `Unlinked` or `Stopped` either waits for this result, or has already
        // closed the account and this result publishes nothing.
        let events = self.events.clone();
        let mut unauthorized = None;
        {
            let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
            if !gate.open || gate.inflight.remove(&request).is_none() {
                return;
            }
            match result {
                Ok(sent) => {
                    let mut message = chat_message(&tracked.conversation_id, tracked.bot_id, &sent);
                    // A send echo with no content still shows the text that was posted.
                    if sent.content.is_empty()
                        && let Some(posted) = self.carried.bodies().get(&tracked.message_id)
                        && !posted.trim().is_empty()
                    {
                        message.body.clone_from(posted);
                    }
                    self.carried.bodies().remove(&tracked.message_id);
                    self.carried
                        .history()
                        .entry(tracked.conversation_id.clone())
                        .or_default()
                        .push(message.id.clone());
                    emit_send_accepted(
                        &events,
                        ProtocolId::Discord,
                        &tracked.conversation_id,
                        request,
                    );
                    emit_message_replaced(&events, tracked.message_id, message);
                }
                Err(error) => {
                    tracing::info!(%error, "discord send failed");
                    emit_message_delivery(
                        &events,
                        ProtocolId::Discord,
                        tracked.conversation_id.clone(),
                        tracked.message_id.clone(),
                        Delivery::Failed,
                    );
                    emit_send_rejected(
                        &events,
                        ProtocolId::Discord,
                        &tracked.conversation_id,
                        request,
                    );
                    if error == DiscordApiError::Unauthorized {
                        unauthorized = Some(error);
                    } else if self.unlink.is_none() && !self.replaced() {
                        emit_ready(&events, &format!("Send failed: {error}."));
                    }
                }
            }
        }
        // Keep this order: the row and `SendRejected` of this send first,
        // then the 401 step, which answers the other sends. The gate is
        // released first: the 401 step may wait.
        if let Some(error) = unauthorized {
            self.settle_unauthorized(error, false);
        }
    }

    /// The 401 step. In one owner step it finishes every load. Sends in
    /// flight get up to [`SEND_SETTLE_WAIT`] for their HTTP result; then the
    /// rest are rejected and the seal starts. `Unlinked` comes when the seal
    /// ends. Commands that come meanwhile are answered and keep the pending
    /// `Unlinked`.
    fn settle_unauthorized(&mut self, error: DiscordApiError, from_reload: bool) {
        if self.unlink.is_some() || self.unlinked() || self.replaced() {
            return;
        }
        self.flags.ending.store(true, Ordering::SeqCst);
        let mut loads: Vec<(u64, String)> = self.loads.drain().collect();
        loads.sort_by_key(|(load, _)| *load);
        let detail = format!("History did not load: {error}.");
        for (_, conversation_id) in loads {
            emit_command_failed(
                &self.events,
                ProtocolId::Discord,
                Some(conversation_id.clone()),
                detail.clone(),
            );
            emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
        }
        self.next_unlink += 1;
        let unlink = self.next_unlink;
        self.unlink = Some(PendingUnlink {
            id: unlink,
            error,
            from_reload,
            sealing: false,
        });
        if !self.account_pending() {
            self.start_seal(unlink);
        } else {
            self.spawn(async move {
                tokio::time::sleep(SEND_SETTLE_WAIT).await;
                Msg::SendWaitOver { unlink }
            });
        }
    }

    /// The wait for sends in flight ended. Reject the rest and settle their
    /// rows: the optimistic row goes, a retried row fails (Codex
    /// r4108983123). Then start the seal.
    fn send_wait_over(&mut self, unlink: u64) {
        if !self
            .unlink
            .as_ref()
            .is_some_and(|pending| pending.id == unlink && !pending.sealing)
        {
            return;
        }
        self.reject_account_sends();
        self.start_seal(unlink);
    }

    fn start_seal(&mut self, unlink: u64) {
        let Some(pending) = self.unlink.as_mut() else {
            return;
        };
        pending.sealing = true;
        let api = Arc::clone(&self.api);
        self.spawn(async move {
            tokio::task::yield_now().await;
            // A test holds the seal here to run commands "after the 401 step".
            if let Some(hold) = api.unlink_pause() {
                hold.notified().await;
            }
            Msg::SealDone { unlink }
        });
    }

    fn seal_done(&mut self, unlink: u64) {
        if self.replaced()
            || !self
                .unlink
                .as_ref()
                .is_some_and(|pending| pending.id == unlink && pending.sealing)
        {
            return;
        }
        let Some(pending) = self.unlink.take() else {
            return;
        };
        // The flags first: the adapter drops the session on its next command.
        self.flags.revoked.store(true, Ordering::SeqCst);
        self.flags.ending.store(false, Ordering::SeqCst);
        let reason = pending.error.reason();
        let from_reload = pending.from_reload;
        let reload_error = pending.error;
        // Hold the gate through the leftover rejects and `Unlinked`, so a
        // predecessor result cannot land after the account ends.
        let events = self.events.clone();
        let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        if !gate.open || gate.epoch != self.epoch {
            return;
        }
        reject_open(&mut self.inflight, &self.carried, &events, &mut gate);
        gate.open = false;
        emit_account(&events, ProtocolId::Discord, AccountState::Unlinked);
        emit_notice(&events, ProtocolId::Discord, reason);
        if from_reload {
            emit_status(
                &events,
                ProtocolId::Discord,
                AdapterStatus::Error,
                format!("Discord bot inbox did not load: {reload_error}"),
            );
        }
    }

    fn finish_shutdown_if_idle(&mut self) {
        if self.shutdown.is_none() || self.send_tasks > 0 {
            return;
        }
        // A predecessor's HTTP task is not in `send_tasks`. Wait for it, then
        // reject whatever is still open, then emit `Stopped`.
        if self.carried_pending() {
            self.arm_settle(AccountEnd::Stopped);
            return;
        }
        self.close();
    }

    /// The user disconnected. Sends in flight get up to [`SEND_SETTLE_WAIT`]
    /// for their real result (Codex r4132922551), then the disconnected state
    /// follows. Loads end at once. A send carried from the previous session
    /// counts: it is answered before `Unlinked`.
    fn generation_is_current(&self, generation: u64) -> bool {
        let newest = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        *newest == generation
    }

    fn disconnect(&mut self, detail: &'static str, attempt: u64) {
        self.retire();
        if !self.generation_is_current(attempt) {
            // A newer session already owns the account. HistoryLoaded,
            // SendRejected, and MessagesRemoved from this disconnect would
            // settle that session (Codex r4133411242).
            self.loads.clear();
            return;
        }
        self.disconnecting = Some((detail, attempt));
        let mut loads: Vec<(u64, String)> = self.loads.drain().collect();
        loads.sort_by_key(|(load, _)| *load);
        for (_, conversation_id) in loads {
            emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
        }
        if self.account_pending() {
            self.spawn(async {
                tokio::time::sleep(SEND_SETTLE_WAIT).await;
                Msg::DisconnectWaitOver
            });
        } else {
            self.finish_disconnect();
        }
    }

    /// Reject the sends still in flight (settle their rows), then emit the
    /// disconnected state. Nothing of this account comes after it. The
    /// generation check and the publication hold the account gate.
    fn finish_disconnect(&mut self) {
        let Some((detail, attempt)) = self.disconnecting.take() else {
            return;
        };
        if self.seal(AccountEnd::Disconnect { detail, attempt }) {
            self.mark_closed();
        }
    }

    /// Ends a shutdown: reject each send still in flight for the account,
    /// including one carried from the previous session, emit `Stopped` once,
    /// and publish nothing more. The gate stays held through that.
    fn close(&mut self) {
        let Some(done) = self.shutdown.take() else {
            return;
        };
        self.retire();
        if !self.seal(AccountEnd::Stopped) {
            // The account already ended. `Stopped` still comes once.
            self.flags.emit_stopped_once(&self.events);
        }
        self.mark_closed();
        let _ = done.send(());
    }

    fn account_settle(&mut self) {
        let Some(end) = self.pending_end.take() else {
            return;
        };
        match end {
            AccountEnd::Stopped => self.close(),
            end => {
                if self.seal(end) {
                    self.mark_closed();
                }
            }
        }
    }

    fn begin_account_end(&mut self, end: AccountEnd) {
        if self.account_pending() {
            self.arm_settle(end);
            return;
        }
        if self.seal(end) {
            self.mark_closed();
        }
    }

    fn arm_settle(&mut self, end: AccountEnd) {
        if self.settle_armed {
            return;
        }
        self.settle_armed = true;
        self.pending_end = Some(end);
        self.spawn(async {
            tokio::time::sleep(SEND_SETTLE_WAIT).await;
            Msg::AccountSettle
        });
    }

    fn account_pending(&self) -> bool {
        !self
            .account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .inflight
            .is_empty()
    }

    /// A send this owner did not start is still in the account gate.
    fn carried_pending(&self) -> bool {
        let gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        gate.inflight
            .keys()
            .any(|request| !self.inflight.contains_key(request))
    }

    /// Answers every send still in the gate, then emits the end. Returns
    /// false when the account is already closed, or this disconnect is no
    /// longer the newest attempt (those sends stay for the session that
    /// took over).
    fn seal(&mut self, end: AccountEnd) -> bool {
        if let AccountEnd::Disconnect { detail, attempt } = end {
            return self.seal_disconnect(detail, attempt);
        }
        let events = self.events.clone();
        let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        if !gate.open || gate.epoch != self.epoch {
            return false;
        }
        reject_open(&mut self.inflight, &self.carried, &events, &mut gate);
        gate.open = false;
        if let AccountEnd::Stopped = end {
            self.flags.emit_stopped_once(&events);
        } else {
            emit_account(&events, ProtocolId::Discord, AccountState::Unlinked);
        }
        true
    }

    /// The generation check and `Unlinked` hold the account gate and
    /// `active`, so a send result cannot pass the end and a newer attempt
    /// cannot lose its status.
    fn seal_disconnect(&mut self, detail: &'static str, attempt: u64) -> bool {
        let events = self.events.clone();
        let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        if !gate.open || gate.epoch != self.epoch {
            return false;
        }
        let newest = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        if *newest != attempt {
            // The wait outlived this disconnect. Leave the sends: their
            // real results still apply to the session that took over
            // (Codex r4132725575, r4132922561, r4133411242).
            return false;
        }
        reject_open(&mut self.inflight, &self.carried, &events, &mut gate);
        gate.open = false;
        emit_disconnected(&events, detail);
        // `newest` stays held through the emit. NLL would drop it at the
        // comparison above, and a newer attempt could land in that gap.
        drop(newest);
        true
    }

    /// Rejects the sends still open. The account stays open, so a result
    /// that arrives before the end can still be accepted.
    fn reject_account_sends(&mut self) {
        let events = self.events.clone();
        let mut gate = self.account.lock().unwrap_or_else(PoisonError::into_inner);
        if !gate.open || gate.epoch != self.epoch {
            return;
        }
        reject_open(&mut self.inflight, &self.carried, &events, &mut gate);
    }

    /// Runs HTTP work off the owner. Its result comes back as a message. The
    /// task holds a strong sender, so a retired owner still gets the result.
    fn mark_closed(&mut self) {
        self.closed = true;
        self.tx_hold = None;
    }

    fn spawn(&self, work: impl Future<Output = Msg> + Send + 'static) {
        let Some(tx) = self.tx.upgrade().or_else(|| self.tx_hold.clone()) else {
            return;
        };
        tokio::spawn(async move {
            let _ = tx.send(work.await);
        });
    }
}

/// Rejects every send still in the gate. The caller holds the gate.
fn reject_open(
    local: &mut HashMap<u64, Inflight>,
    carried: &Carried,
    events: &EventTx,
    gate: &mut AccountSends,
) {
    let mut sends: Vec<(u64, Inflight)> = gate.inflight.drain().collect();
    sends.sort_by_key(|(request, _)| *request);
    for (request, tracked) in sends {
        local.remove(&request);
        if tracked.row == SendRow::Pending {
            carried.bodies().remove(&tracked.message_id);
        }
        settle_row(events, &tracked);
        emit_send_rejected(
            events,
            ProtocolId::Discord,
            &tracked.conversation_id,
            request,
        );
    }
}

/// A send that gets no accepted result: remove its optimistic row, or set a
/// retried row back to failed.
fn settle_row(events: &EventTx, tracked: &Inflight) {
    match tracked.row {
        SendRow::Pending => {
            let _ = events.send(AdapterEvent::MessagesRemoved {
                protocol: ProtocolId::Discord,
                conversation_id: tracked.conversation_id.clone(),
                message_ids: vec![tracked.message_id.clone()],
            });
        }
        SendRow::Retry => emit_message_delivery(
            events,
            ProtocolId::Discord,
            tracked.conversation_id.clone(),
            tracked.message_id.clone(),
            Delivery::Failed,
        ),
    }
}

/// The disconnected state: a stubbed status with `detail`, then `Unlinked`.
fn emit_disconnected(events: &EventTx, detail: &str) {
    emit_status(events, ProtocolId::Discord, AdapterStatus::Stubbed, detail);
    emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
}

fn emit_ready(events: &EventTx, note: &str) {
    emit_status(
        events,
        ProtocolId::Discord,
        AdapterStatus::Ready,
        format!("Discord bot inbox. {note} {BOT_TOKEN_PRESENT}. Not a personal Discord client."),
    );
}
