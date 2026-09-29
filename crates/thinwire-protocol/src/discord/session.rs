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
//! and `retired`. The owner sets the first three. [`Session::retire`] sets
//! `retired` before it returns, so a list or history result already queued
//! publishes nothing.
//!
//! State diagram: `thinwire-team/discord.md`, section "Session owner".

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
#[derive(Debug, Default)]
pub(crate) struct Carried {
    /// Channel ids from the last list. The next list removes gone rows.
    channels: HashMap<String, ChannelAccess>,
    /// Message ids from the last history page of each channel. Without them
    /// the next page cannot remove rows the shell still shows.
    history: HashMap<String, Vec<String>>,
    /// Texts of outgoing rows, so Retry still works after a reconnect.
    bodies: HashMap<String, String>,
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
    /// [`SEND_SETTLE_WAIT`] of the 401 step `unlink` ended.
    SendWaitOver {
        unlink: u64,
    },
    /// The seal of the 401 step `unlink` ended: queue `Unlinked` now.
    SealDone {
        unlink: u64,
    },
    Retire {
        carried: oneshot::Sender<Carried>,
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
    /// Set by [`Session::retire`] before it returns. A completion already
    /// queued still sees it, so disconnect can emit `Unlinked` without a
    /// later `Linked`, conversation, or history row (Codex r4131954091).
    retired: AtomicBool,
}

impl Flags {
    fn emit_stopped_once(&self, events: &EventTx) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            emit_stopped(events, ProtocolId::Discord);
        }
    }
}

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
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let flags = Arc::new(Flags::default());
        emit_account(events, ProtocolId::Discord, AccountState::Linking);
        emit_status(
            events,
            ProtocolId::Discord,
            AdapterStatus::Connecting,
            format!("Discord bot inbox is loading guild channels. {BOT_TOKEN_PRESENT}."),
        );
        let owner = Owner {
            api,
            events: events.clone(),
            tx: tx.downgrade(),
            flags: Arc::clone(&flags),
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
            closed: false,
            send_tasks: 0,
            shutdown: None,
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

    /// A later session replaces this one, or the user disconnects. Results
    /// that are still running are dropped; a send still in flight is rejected
    /// when its HTTP call ends. The receiver gets the state for the next
    /// session.
    ///
    /// Retirement is visible before this returns. A [`Msg::ReloadDone`] or
    /// [`Msg::HistoryDone`] already queued publishes nothing, even though the
    /// owner handles that message before [`Msg::Retire`].
    pub(crate) fn retire(self) -> oneshot::Receiver<Carried> {
        self.flags.retired.store(true, Ordering::SeqCst);
        let (carried, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Retire { carried });
        rx
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

/// The end of a shutdown. See [`Session::shutdown`].
pub(crate) struct Closing {
    done: oneshot::Receiver<()>,
    flags: Arc<Flags>,
    limit: Duration,
}

impl Closing {
    /// Extra time over the limit before the adapter stops waiting for a
    /// stuck owner.
    const MARGIN: Duration = Duration::from_secs(1);

    pub(crate) async fn wait(self, events: &EventTx) {
        let closed = tokio::time::timeout(self.limit + Self::MARGIN, self.done).await;
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
    flags: Arc<Flags>,
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
    /// The app closed: `Stopped` is queued and nothing more is published.
    closed: bool,
    /// HTTP send tasks still running. Shutdown waits for zero.
    send_tasks: u64,
    shutdown: Option<oneshot::Sender<()>>,
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
            Msg::Retire { carried } => {
                self.retire();
                let _ = carried.send(std::mem::take(&mut self.carried));
            }
            Msg::Shutdown { done, limit } => {
                self.shutdown = Some(done);
                self.spawn(async move {
                    tokio::time::sleep(limit).await;
                    Msg::ShutdownLimit
                });
                self.finish_shutdown_if_idle();
            }
            Msg::ShutdownLimit => self.close(),
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

    /// [`Session::retire`] has returned, or this task has handled `Retire`.
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
        let reject = |owner: &Self| {
            emit_send_rejected(
                &owner.events,
                ProtocolId::Discord,
                &conversation_id,
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
        if self.retired || self.unlink.is_some() || self.unlinked() {
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
        let Some(tx) = self.tx.upgrade() else {
            reject(self);
            return;
        };
        let (body, message_id, row) = match outgoing {
            Outgoing::New(body) => {
                self.next_pending += 1;
                let message_id = format!("discord:pending:{}:{}", self.id, self.next_pending);
                self.carried.bodies.insert(message_id.clone(), body.clone());
                emit_message(
                    &self.events,
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
                let Some(body) = self
                    .carried
                    .bodies
                    .get(&message_id)
                    .cloned()
                    .filter(|text| !text.trim().is_empty())
                else {
                    reject(self);
                    return;
                };
                (body, message_id, SendRow::Retry)
            }
        };
        self.inflight.insert(
            request,
            Inflight {
                conversation_id,
                message_id,
                row,
                bot_id,
            },
        );
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
            // A load that finishes after `Retire` was handled still ends the
            // spinner. One already queued when `retire` returned publishes
            // nothing: disconnect may already have emitted `Unlinked`.
            if self.retired && !self.unlinked() {
                emit_history_loaded(&self.events, ProtocolId::Discord, conversation_id);
            }
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
                let previous = self
                    .carried
                    .history
                    .entry(conversation_id.clone())
                    .or_default();
                let gone: Vec<String> = previous
                    .iter()
                    .filter(|id| !new_ids.iter().any(|next| next == *id))
                    .cloned()
                    .collect();
                *previous = new_ids;
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
        if self.inflight.is_empty()
            && let Some(unlink) = &self.unlink
            && !unlink.sealing
        {
            let id = unlink.id;
            self.start_seal(id);
        }
        self.finish_shutdown_if_idle();
    }

    fn answer_send(&mut self, request: u64, result: Result<MessageSummary, DiscordApiError>) {
        // The 401 step or an earlier result already answered this request.
        let Some(tracked) = self.inflight.remove(&request) else {
            return;
        };
        if self.retired {
            // A later session replaced this one: the row does not stay pending.
            settle_row(&self.events, &tracked);
            emit_send_rejected(
                &self.events,
                ProtocolId::Discord,
                &tracked.conversation_id,
                request,
            );
            return;
        }
        match result {
            Ok(sent) => {
                let mut message = chat_message(&tracked.conversation_id, tracked.bot_id, &sent);
                // A send echo with no content still shows the text that was posted.
                if sent.content.is_empty()
                    && let Some(posted) = self.carried.bodies.get(&tracked.message_id)
                    && !posted.trim().is_empty()
                {
                    message.body.clone_from(posted);
                }
                self.carried.bodies.remove(&tracked.message_id);
                self.carried
                    .history
                    .entry(tracked.conversation_id.clone())
                    .or_default()
                    .push(message.id.clone());
                emit_send_accepted(
                    &self.events,
                    ProtocolId::Discord,
                    &tracked.conversation_id,
                    request,
                );
                emit_message_replaced(&self.events, tracked.message_id, message);
            }
            Err(error) => {
                tracing::info!(%error, "discord send failed");
                emit_message_delivery(
                    &self.events,
                    ProtocolId::Discord,
                    tracked.conversation_id.clone(),
                    tracked.message_id.clone(),
                    Delivery::Failed,
                );
                emit_send_rejected(
                    &self.events,
                    ProtocolId::Discord,
                    &tracked.conversation_id,
                    request,
                );
                if error == DiscordApiError::Unauthorized {
                    self.settle_unauthorized(error, false);
                } else if self.unlink.is_none() {
                    emit_ready(&self.events, &format!("Send failed: {error}."));
                }
            }
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
        if self.inflight.is_empty() {
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
        let mut sends: Vec<(u64, Inflight)> = self.inflight.drain().collect();
        sends.sort_by_key(|(request, _)| *request);
        for (request, tracked) in sends {
            settle_row(&self.events, &tracked);
            emit_send_rejected(
                &self.events,
                ProtocolId::Discord,
                &tracked.conversation_id,
                request,
            );
        }
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
        emit_account(&self.events, ProtocolId::Discord, AccountState::Unlinked);
        emit_notice(&self.events, ProtocolId::Discord, pending.error.reason());
        if pending.from_reload {
            emit_status(
                &self.events,
                ProtocolId::Discord,
                AdapterStatus::Error,
                format!("Discord bot inbox did not load: {}", pending.error),
            );
        }
    }

    fn finish_shutdown_if_idle(&mut self) {
        if self.send_tasks == 0 {
            self.close();
        }
    }

    /// Ends a shutdown: retire, emit `Stopped` once, and publish nothing
    /// more, also when sends are still in flight after the limit.
    fn close(&mut self) {
        let Some(done) = self.shutdown.take() else {
            return;
        };
        self.retire();
        self.closed = true;
        self.flags.emit_stopped_once(&self.events);
        let _ = done.send(());
    }

    /// Runs HTTP work off the owner. Its result comes back as a message. The
    /// task holds a strong sender, so a retired owner still gets the result.
    fn spawn(&self, work: impl Future<Output = Msg> + Send + 'static) {
        let Some(tx) = self.tx.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            let _ = tx.send(work.await);
        });
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

fn emit_ready(events: &EventTx, note: &str) {
    emit_status(
        events,
        ProtocolId::Discord,
        AdapterStatus::Ready,
        format!("Discord bot inbox. {note} {BOT_TOKEN_PRESENT}. Not a personal Discord client."),
    );
}
