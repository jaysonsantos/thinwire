//! Live bot inbox session. Every HTTP call runs in a tokio task.
//!
//! A generation counter drops results from a session that a later connect or a
//! disconnect replaced. Only channels from the last list may open or send.
//!
//! After any await, re-check that generation under the session lock before
//! queuing an event or changing session state. The value captured before the
//! await is not enough.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::api::{DiscordApi, DiscordApiError, MessageSummary};
use super::inbox::{HISTORY_LIMIT, InboxChannel, chat_message, load_channels};
use super::{BOT_TOKEN_PRESENT, UNKNOWN_CHANNEL_REFUSAL};
use crate::adapter::{
    AccountState, AdapterError, AdapterEvent, AdapterStatus, ChatMessage, Delivery, EventTx,
    ProtocolId, emit_account, emit_chat_list_loaded, emit_command_failed, emit_conversation,
    emit_conversation_removed, emit_history_loaded, emit_message, emit_message_delivery,
    emit_message_replaced, emit_notice, emit_send_accepted, emit_send_rejected, emit_status,
};

const READ_ONLY_REFUSAL: &str = "The bot does not have Send Messages in that channel.";

/// Unix seconds on the local clock. A pending row uses this so it sorts after
/// history until Discord's snowflake time replaces it.
fn local_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChannelAccess {
    channel_id: u64,
    can_send: bool,
}

#[derive(Debug, Clone)]
struct Inflight {
    conversation_id: String,
    request: u64,
}

#[derive(Debug)]
struct PendingLoad {
    conversation_id: String,
}

/// One lock for revocation and in-flight work.
///
/// Invariants, held for the whole time this mutex is locked:
/// - `revoked` flips to true only in the 401 settle step, before `Unlinked` is queued.
/// - `inflight` is the pending-send map. A send stays there until its one
///   `SendAccepted` or `SendRejected` is queued, or the 401 step queues that
///   `SendRejected`.
/// - After `revoked`, starting a send queues `SendRejected` and does not insert.
/// - A 401 step queues one result for every send still in the map, finishes
///   every load in `loads`, then queues `Unlinked`, before the lock is released.
/// - A task that observes `revoked`, or no longer finds its entry, queues nothing.
/// - A channel-list reload publishes `Linked`, the rows, and `ChatListLoaded`
///   in this same step, and queues none of them if the session is already revoked.
/// - `generation` is this session. Reload, send, and history results carry the
///   value they started with. If it moved, the result is dropped in this step
///   and does not unlink whatever session replaced it. Channel previews are
///   part of the reload result on this path, not a second task.
/// - Once `revoked` is set, a send is not inserted. `register_send` queues
///   `SendRejected` in that step, and if `Unlinked` is not queued yet it queues
///   that rejection first. No send is registered for a revoked generation.
/// - The shared live counter moves only in this step, and only after the
///   generation check: replacing the session, or publishing `Unlinked`.
#[derive(Debug, Default)]
struct Shared {
    bot_id: Option<u64>,
    channels: HashMap<String, ChannelAccess>,
    inflight: HashMap<u64, Inflight>,
    /// History loads still running. A 401 finishes them before `Unlinked`.
    loads: Vec<PendingLoad>,
    /// Message ids from the last history page for each channel.
    history: HashMap<String, Vec<String>>,
    /// Body of each outgoing row, so a retry can post it again.
    bodies: HashMap<String, String>,
    /// Latest `reload` in this session. An older list must not publish.
    reload_ticket: u64,
    /// Bumped when a later connect replaces this session. In-flight results
    /// captured the old value and are dropped when it no longer matches.
    generation: u64,
    /// A 401 revoked the token. The adapter drops this session on the next command.
    revoked: bool,
    /// A 401 rejected tracked work and has not queued `Unlinked` yet.
    /// `register_send` puts its `SendRejected` ahead of that event.
    pending_unlink: Option<DiscordApiError>,
    /// Send tasks still running. Shutdown waits until this is zero.
    send_tasks: u64,
    send_idle: Arc<Notify>,
}

/// Drops events from a replaced session.
#[derive(Clone)]
struct Gate {
    live: Arc<AtomicU64>,
    generation: u64,
}

impl Gate {
    fn current(&self) -> bool {
        self.live.load(Ordering::SeqCst) == self.generation
    }

    /// Later tasks from this session must not publish inbox events.
    fn invalidate(&self) {
        self.live.fetch_add(1, Ordering::SeqCst);
    }
}

pub(crate) struct Session {
    api: Arc<dyn DiscordApi>,
    shared: Arc<Mutex<Shared>>,
    gate: Gate,
    next_pending: u64,
}

impl Session {
    /// Channel ids from the last list. A later connect uses them to drop gone rows.
    pub(crate) fn carried_channels(&self) -> HashMap<String, ChannelAccess> {
        self.shared
            .lock()
            .map(|state| state.channels.clone())
            .unwrap_or_default()
    }

    /// Message ids from the last history page of each channel.
    ///
    /// A replacement session starts empty. Without these ids the next page
    /// cannot remove rows the shell still shows.
    pub(crate) fn carried_history(&self) -> HashMap<String, Vec<String>> {
        self.shared
            .lock()
            .map(|state| state.history.clone())
            .unwrap_or_default()
    }

    /// Bodies of outgoing rows, so Retry still works after a reconnect.
    pub(crate) fn carried_bodies(&self) -> HashMap<String, String> {
        self.shared
            .lock()
            .map(|state| state.bodies.clone())
            .unwrap_or_default()
    }

    /// True after a 401. The adapter drops the session on the next command.
    pub(crate) fn is_revoked(&self) -> bool {
        self.shared.lock().is_ok_and(|state| state.revoked)
    }

    /// Starts a new generation and loads the channel list.
    ///
    /// `carried` is the previous list. An empty map is a first connect.
    /// The live-counter bump happens while this session's lock is held, before
    /// any task can publish.
    pub(crate) fn start(
        api: Arc<dyn DiscordApi>,
        live: &Arc<AtomicU64>,
        events: &EventTx,
        carried: HashMap<String, ChannelAccess>,
        history: HashMap<String, Vec<String>>,
        bodies: HashMap<String, String>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            bot_id: None,
            channels: carried,
            inflight: HashMap::new(),
            loads: Vec::new(),
            history,
            bodies,
            reload_ticket: 0,
            generation: 0,
            revoked: false,
            pending_unlink: None,
            send_tasks: 0,
            send_idle: Arc::new(Notify::new()),
        }));
        let generation = {
            let mut state = shared.lock().unwrap_or_else(|err| err.into_inner());
            let generation = live.fetch_add(1, Ordering::SeqCst) + 1;
            state.generation = generation;
            generation
        };
        let session = Self {
            api,
            shared,
            gate: Gate {
                live: Arc::clone(live),
                generation,
            },
            next_pending: 0,
        };
        emit_account(events, ProtocolId::Discord, AccountState::Linking);
        emit_status(
            events,
            ProtocolId::Discord,
            AdapterStatus::Connecting,
            format!("Discord bot inbox is loading guild channels. {BOT_TOKEN_PRESENT}."),
        );
        session.reload(events);
        session
    }

    /// Loads the guild channel list again. Channels that left the list are removed.
    pub(crate) fn reload(&self, events: &EventTx) {
        let ticket = {
            let Ok(mut state) = self.shared.lock() else {
                return;
            };
            if state.revoked {
                return;
            }
            state.reload_ticket = state.reload_ticket.wrapping_add(1);
            state.reload_ticket
        };
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let generation = self.gate.generation;
        let events = events.clone();
        tokio::spawn(async move {
            let result = load_channels(api.as_ref()).await;
            match result {
                Ok((bot_id, channels)) => {
                    // Publication rechecks the generation, the gate, the ticket,
                    // and `revoked` under the session lock.
                    publish_channels(
                        &shared, &gate, generation, ticket, &events, bot_id, &channels,
                    );
                }
                Err(error) => {
                    tracing::info!(%error, "discord channel list failed");
                    if finish_reload(&shared, generation, ticket, &events, error) {
                        let sealed = seal_unlink(api.as_ref(), generation).await;
                        publish_unlink(&shared, &gate, sealed, &events);
                        note_reload_error(&shared, sealed, &events, error);
                    }
                }
            }
        });
    }

    /// Loads recent messages for a listed channel, oldest first.
    pub(crate) fn open(
        &self,
        conversation_id: String,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        let (access, bot_id) = {
            let Ok(mut state) = self.shared.lock() else {
                return Err(refused_channel());
            };
            if state.revoked {
                return Ok(());
            }
            let (access, bot_id) = match (state.channels.get(&conversation_id), state.bot_id) {
                (Some(access), Some(bot_id)) => (*access, bot_id),
                (Some(_), None) => {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Discord,
                        reason: "Discord bot inbox is still loading guild channels.",
                    });
                }
                _ => {
                    emit_notice(events, ProtocolId::Discord, UNKNOWN_CHANNEL_REFUSAL);
                    emit_command_failed(
                        events,
                        ProtocolId::Discord,
                        Some(conversation_id.clone()),
                        UNKNOWN_CHANNEL_REFUSAL,
                    );
                    emit_history_loaded(events, ProtocolId::Discord, conversation_id);
                    return Ok(());
                }
            };
            state.loads.push(PendingLoad {
                conversation_id: conversation_id.clone(),
            });
            (access, bot_id)
        };
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let generation = self.gate.generation;
        let events = events.clone();
        tokio::spawn(async move {
            let result = api.history(access.channel_id, HISTORY_LIMIT).await;
            // The await released the lock. A newer connect may own the generation.
            if !same_generation(&shared, generation) {
                finish_replaced_load(&shared, &events, conversation_id);
                return;
            }
            match result {
                Ok(messages) => {
                    let rows: Vec<ChatMessage> = messages
                        .iter()
                        .rev()
                        .map(|message| chat_message(&conversation_id, bot_id, message))
                        .collect();
                    let new_ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
                    let Ok(mut state) = shared.lock() else {
                        drop_load(&shared, &conversation_id);
                        emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                        return;
                    };
                    if state.generation != generation || state.revoked || !gate.current() {
                        drop(state);
                        finish_replaced_load(&shared, &events, conversation_id);
                        return;
                    }
                    drop_load_locked(&mut state, &conversation_id);
                    let previous = state.history.entry(conversation_id.clone()).or_default();
                    let gone: Vec<String> = previous
                        .iter()
                        .filter(|id| !new_ids.iter().any(|next| next == *id))
                        .cloned()
                        .collect();
                    *previous = new_ids;
                    if !gone.is_empty() {
                        let _ = events.send(AdapterEvent::MessagesRemoved {
                            protocol: ProtocolId::Discord,
                            conversation_id: conversation_id.clone(),
                            message_ids: gone,
                        });
                    }
                    for row in rows {
                        emit_message(&events, row);
                    }
                    emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                }
                Err(error) => {
                    tracing::info!(%error, "discord history failed");
                    let unauthorized = {
                        let Ok(mut state) = shared.lock() else {
                            drop_load(&shared, &conversation_id);
                            emit_command_failed(
                                &events,
                                ProtocolId::Discord,
                                Some(conversation_id.clone()),
                                format!("History did not load: {error}."),
                            );
                            emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                            return;
                        };
                        if state.generation != generation {
                            drop(state);
                            finish_replaced_load(&shared, &events, conversation_id);
                            return;
                        }
                        if error == DiscordApiError::Unauthorized {
                            if !state.revoked {
                                settle_unauthorized(&mut state, &events, error);
                            }
                            true
                        } else if state.revoked {
                            false
                        } else {
                            drop_load_locked(&mut state, &conversation_id);
                            emit_command_failed(
                                &events,
                                ProtocolId::Discord,
                                Some(conversation_id.clone()),
                                format!("History did not load: {error}."),
                            );
                            emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                            false
                        }
                    };
                    if unauthorized {
                        let sealed = seal_unlink(api.as_ref(), generation).await;
                        publish_unlink(&shared, &gate, sealed, &events);
                    }
                }
            }
        });
        Ok(())
    }

    /// Sends as the bot. A pending row shows at once and is replaced on success.
    ///
    /// `request` is the UI's send id. Acceptance is the HTTP success, not the
    /// optimistic row. A failure names that id in `SendRejected` so the draft stays.
    pub(crate) fn send(
        &mut self,
        conversation_id: String,
        body: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        let Some(registered) =
            self.register_send(&conversation_id, request, events, Outgoing::New(body))?
        else {
            return Ok(());
        };
        self.spawn_send(conversation_id, request, events, registered);
        Ok(())
    }

    /// Posts a failed outgoing row again. The shell names that row and a new request.
    pub(crate) fn resend(
        &mut self,
        conversation_id: String,
        message_id: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        let Some(registered) = self.register_send(
            &conversation_id,
            request,
            events,
            Outgoing::Retry(message_id),
        )?
        else {
            return Ok(());
        };
        self.spawn_send(conversation_id, request, events, registered);
        Ok(())
    }

    /// Checks revocation and records the send under one lock. `None` means the
    /// result event is already queued.
    fn register_send(
        &mut self,
        conversation_id: &str,
        request: u64,
        events: &EventTx,
        outgoing: Outgoing,
    ) -> Result<Option<RegisteredSend>, AdapterError> {
        let pending_id = match &outgoing {
            Outgoing::New(_) => {
                self.next_pending += 1;
                Some(format!(
                    "discord:pending:{}:{}",
                    self.gate.generation, self.next_pending
                ))
            }
            Outgoing::Retry(_) => None,
        };
        let Ok(mut state) = self.shared.lock() else {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            return Ok(None);
        };
        if state.revoked {
            // This generation is already revoked. Reject here and do not insert.
            // `Unlinked` is queued after this rejection, unless a reconnect
            // moved the generation before `publish_unlink` re-takes the lock.
            let generation = self.gate.generation;
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            drop(state);
            publish_unlink(&self.shared, &self.gate, generation, events);
            return Ok(None);
        }
        let (access, bot_id) = match (state.channels.get(conversation_id), state.bot_id) {
            (Some(access), Some(bot_id)) => (*access, bot_id),
            (Some(_), None) => {
                return Err(AdapterError::Unavailable {
                    protocol: ProtocolId::Discord,
                    reason: "Discord bot inbox is still loading guild channels.",
                });
            }
            _ => {
                emit_notice(events, ProtocolId::Discord, UNKNOWN_CHANNEL_REFUSAL);
                emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
                return Ok(None);
            }
        };
        if !access.can_send {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            emit_notice(events, ProtocolId::Discord, READ_ONLY_REFUSAL);
            return Ok(None);
        }
        let (body, message_id, row) = match outgoing {
            Outgoing::New(body) => {
                let message_id = pending_id.expect("a new send has a pending id");
                state.bodies.insert(message_id.clone(), body.clone());
                state.inflight.insert(
                    request,
                    Inflight {
                        conversation_id: conversation_id.to_owned(),
                        request,
                    },
                );
                emit_message(
                    events,
                    ChatMessage {
                        protocol: ProtocolId::Discord,
                        conversation_id: conversation_id.to_owned(),
                        id: message_id.clone(),
                        sender: "bot".into(),
                        body: body.clone(),
                        outbound: true,
                        delivery: Delivery::Pending,
                        sent_at: local_unix_seconds(),
                    },
                );
                (body, message_id, SendRow::Pending)
            }
            Outgoing::Retry(message_id) => {
                let Some(body) = state.bodies.get(&message_id).cloned() else {
                    emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
                    return Ok(None);
                };
                state.inflight.insert(
                    request,
                    Inflight {
                        conversation_id: conversation_id.to_owned(),
                        request,
                    },
                );
                (body, message_id, SendRow::Retry)
            }
        };
        Ok(Some(RegisteredSend {
            access,
            bot_id,
            body,
            message_id,
            row,
        }))
    }

    fn spawn_send(
        &self,
        conversation_id: String,
        request: u64,
        events: &EventTx,
        registered: RegisteredSend,
    ) {
        let RegisteredSend {
            access,
            bot_id,
            body,
            message_id,
            row,
        } = registered;
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let generation = self.gate.generation;
        let events = events.clone();
        let guard = self.track_send();
        tokio::spawn(async move {
            let _guard = guard;
            let result = api.send(access.channel_id, body).await;
            finish_send(
                api.as_ref(),
                &shared,
                &gate,
                &events,
                ReturnedSend {
                    conversation_id,
                    message_id,
                    request,
                    bot_id,
                    row,
                    generation,
                    result,
                },
            )
            .await;
        });
    }

    /// Moves this session's generation so a result that already started is dropped.
    ///
    /// The shared live counter moves in this same step, and only when the
    /// generation still matches. A stale task cannot bump it later.
    pub(crate) fn retire(&self) {
        let Ok(mut state) = self.shared.lock() else {
            return;
        };
        if state.generation != self.gate.generation {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        self.gate.invalidate();
    }
}

fn refused_channel() -> AdapterError {
    AdapterError::Refused {
        protocol: ProtocolId::Discord,
        reason: UNKNOWN_CHANNEL_REFUSAL,
    }
}

fn publish_channels(
    shared: &Mutex<Shared>,
    gate: &Gate,
    generation: u64,
    ticket: u64,
    events: &EventTx,
    bot_id: u64,
    channels: &[InboxChannel],
) {
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
    let Ok(mut state) = shared.lock() else {
        return;
    };
    if state.generation != generation
        || state.revoked
        || !gate.current()
        || state.reload_ticket != ticket
    {
        return;
    }
    let gone: Vec<String> = state
        .channels
        .keys()
        .filter(|id| !next.contains_key(*id))
        .cloned()
        .collect();
    state.bot_id = Some(bot_id);
    state.channels = next;
    emit_account(events, ProtocolId::Discord, AccountState::Linked);
    // Ready before the rows. The shell opens a chat only once the account is linked.
    emit_ready(
        events,
        &format!("{} guild channels the bot can read.", channels.len()),
    );
    for id in gone {
        emit_conversation_removed(events, ProtocolId::Discord, id);
    }
    for channel in channels {
        emit_conversation(events, channel.conversation());
    }
    // The shell stops the chat-list spinner on this event
    // (adapter contract rule 9, ADR 0010).
    emit_chat_list_loaded(events, ProtocolId::Discord);
}

/// Applies a finished channel-list reload under the session lock.
///
/// A 401 from this generation unlinks. A result whose generation already
/// moved, or whose reload ticket is no longer current, is dropped. It does
/// not emit `CommandFailed` or unlink the list that replaced it.
/// `true` when this generation's current ticket failed with 401. The caller
/// then seals `Unlinked` and emits the error status, after a send waiting on
/// the lock has queued its `SendRejected`.
fn finish_reload(
    shared: &Arc<Mutex<Shared>>,
    generation: u64,
    ticket: u64,
    events: &EventTx,
    error: DiscordApiError,
) -> bool {
    let Ok(mut state) = shared.lock() else {
        return false;
    };
    if state.generation != generation || state.reload_ticket != ticket {
        return false;
    }
    if error == DiscordApiError::Unauthorized {
        if !state.revoked {
            settle_unauthorized(&mut state, events, error);
        }
        return true;
    }
    emit_command_failed(
        events,
        ProtocolId::Discord,
        None,
        format!("Discord bot inbox did not load: {error}"),
    );
    emit_status(
        events,
        ProtocolId::Discord,
        AdapterStatus::Error,
        format!("Discord bot inbox did not load: {error}"),
    );
    false
}

/// Caller holds the session lock. Queues every still-tracked result and leaves
/// `Unlinked` pending, so a `register_send` that takes the lock next can queue
/// its `SendRejected` first.
fn settle_unauthorized(state: &mut Shared, events: &EventTx, error: DiscordApiError) {
    state.revoked = true;
    let sends = std::mem::take(&mut state.inflight);
    let loads = std::mem::take(&mut state.loads);
    for send in sends.into_values() {
        emit_send_rejected(
            events,
            ProtocolId::Discord,
            &send.conversation_id,
            send.request,
        );
    }
    let detail = format!("History did not load: {error}.");
    for load in loads {
        emit_command_failed(
            events,
            ProtocolId::Discord,
            Some(load.conversation_id.clone()),
            detail.clone(),
        );
        emit_history_loaded(events, ProtocolId::Discord, load.conversation_id);
    }
    if state.pending_unlink.is_none() {
        state.pending_unlink = Some(error);
    }
}

/// Queues `Unlinked` for `generation`. Takes the session lock itself and
/// drops the unlink when that generation is no longer current.
fn publish_unlink(shared: &Arc<Mutex<Shared>>, gate: &Gate, generation: u64, events: &EventTx) {
    let Ok(mut state) = shared.lock() else {
        return;
    };
    if state.generation != generation {
        state.pending_unlink = None;
        return;
    }
    let Some(error) = state.pending_unlink.take() else {
        return;
    };
    emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
    emit_notice(events, ProtocolId::Discord, error.reason());
    gate.invalidate();
}

/// Waits out the 401 seal. Returns the generation that was current when the
/// wait started. The caller passes it to [`publish_unlink`], which checks it
/// again under the lock.
async fn seal_unlink(api: &dyn DiscordApi, generation: u64) -> u64 {
    tokio::task::yield_now().await;
    if let Some(hold) = api.unlink_pause() {
        hold.notified().await;
    }
    generation
}

fn same_generation(shared: &Arc<Mutex<Shared>>, generation: u64) -> bool {
    shared
        .lock()
        .is_ok_and(|state| state.generation == generation)
}

/// Error status for a list 401, only while `generation` is still current.
fn note_reload_error(
    shared: &Arc<Mutex<Shared>>,
    generation: u64,
    events: &EventTx,
    error: DiscordApiError,
) {
    let Ok(state) = shared.lock() else {
        return;
    };
    if state.generation != generation {
        return;
    }
    emit_status(
        events,
        ProtocolId::Discord,
        AdapterStatus::Error,
        format!("Discord bot inbox did not load: {error}"),
    );
}

fn session_revoked(shared: &Arc<Mutex<Shared>>) -> bool {
    shared.lock().map(|state| state.revoked).unwrap_or(true)
}

/// Optimistic row (`Pending`) or a retry of a row the shell already has.
enum SendRow {
    Pending,
    Retry,
}

enum Outgoing {
    New(String),
    Retry(String),
}

struct RegisteredSend {
    access: ChannelAccess,
    bot_id: u64,
    body: String,
    message_id: String,
    row: SendRow,
}

struct ReturnedSend {
    conversation_id: String,
    message_id: String,
    request: u64,
    bot_id: u64,
    row: SendRow,
    generation: u64,
    result: Result<MessageSummary, DiscordApiError>,
}

fn drop_load(shared: &Arc<Mutex<Shared>>, conversation_id: &str) {
    if let Ok(mut state) = shared.lock() {
        drop_load_locked(&mut state, conversation_id);
    }
}

fn drop_load_locked(state: &mut Shared, conversation_id: &str) {
    state
        .loads
        .retain(|load| load.conversation_id != conversation_id);
}

/// A replaced load still ends the spinner. A revoked session already did.
fn finish_replaced_load(shared: &Arc<Mutex<Shared>>, events: &EventTx, conversation_id: String) {
    drop_load(shared, &conversation_id);
    if session_revoked(shared) {
        return;
    }
    emit_history_loaded(events, ProtocolId::Discord, conversation_id);
}

/// Applies one HTTP send result. Removal from `inflight` and the result event
/// happen before the session lock is released, so a 401 settle either sees
/// this send or finds its event already queued. Never both.
async fn finish_send(
    api: &dyn DiscordApi,
    shared: &Arc<Mutex<Shared>>,
    gate: &Gate,
    events: &EventTx,
    returned: ReturnedSend,
) {
    let generation = returned.generation;
    let queued = queue_send_result(shared, gate, events, returned);
    let needs_seal = shared
        .lock()
        .is_ok_and(|state| state.pending_unlink.is_some());
    if needs_seal {
        let sealed = seal_unlink(api, generation).await;
        publish_unlink(shared, gate, sealed, events);
    }
    if queued {
        // The result is already on the channel. A test can run a 401 here.
        // The wait publishes nothing after it returns.
        if let Some(pause) = api.send_result_pause() {
            pause.arrived.notify_one();
            pause.release.notified().await;
        }
    }
}

/// `true` when this task queued the one result for the request.
fn queue_send_result(
    shared: &Arc<Mutex<Shared>>,
    gate: &Gate,
    events: &EventTx,
    returned: ReturnedSend,
) -> bool {
    let Ok(mut state) = shared.lock() else {
        return false;
    };
    let ReturnedSend {
        conversation_id,
        message_id,
        request,
        bot_id,
        row,
        generation,
        result,
    } = returned;
    if state.revoked {
        return false;
    }
    let Some(tracked) = state.inflight.remove(&request) else {
        return false;
    };
    if tracked.conversation_id != conversation_id {
        state.inflight.insert(request, tracked);
        return false;
    }
    if state.generation != generation || !gate.current() {
        match row {
            SendRow::Pending => {
                let _ = events.send(AdapterEvent::MessagesRemoved {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id.clone(),
                    message_ids: vec![message_id],
                });
            }
            SendRow::Retry => emit_message_delivery(
                events,
                ProtocolId::Discord,
                conversation_id.clone(),
                message_id,
                Delivery::Failed,
            ),
        }
        emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
        return true;
    }
    match result {
        Ok(sent) => {
            let message = chat_message(&conversation_id, bot_id, &sent);
            state.bodies.remove(&message_id);
            state
                .history
                .entry(conversation_id.clone())
                .or_default()
                .push(message.id.clone());
            emit_send_accepted(events, ProtocolId::Discord, &conversation_id, request);
            emit_message_replaced(events, message_id, message);
        }
        Err(error) => {
            tracing::info!(%error, "discord send failed");
            emit_message_delivery(
                events,
                ProtocolId::Discord,
                conversation_id.clone(),
                message_id,
                Delivery::Failed,
            );
            emit_send_rejected(events, ProtocolId::Discord, &conversation_id, request);
            if error == DiscordApiError::Unauthorized {
                settle_unauthorized(&mut state, events, error);
            } else {
                emit_ready(events, &format!("Send failed: {error}."));
            }
        }
    }
    true
}

/// Completes when every send task started on this session has finished.
async fn settle_sends(shared: &Arc<Mutex<Shared>>) {
    loop {
        let idle = {
            let Ok(state) = shared.lock() else {
                return;
            };
            Arc::clone(&state.send_idle)
        };
        let wait = idle.notified();
        let busy = shared
            .lock()
            .map(|state| state.send_tasks > 0)
            .unwrap_or(false);
        if !busy {
            return;
        }
        wait.await;
    }
}

struct SendGuard {
    shared: Arc<Mutex<Shared>>,
}

impl Session {
    fn track_send(&self) -> SendGuard {
        if let Ok(mut state) = self.shared.lock() {
            state.send_tasks += 1;
        }
        SendGuard {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn wait_for_sends(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move { settle_sends(&shared).await })
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let Ok(mut state) = self.shared.lock() else {
            return;
        };
        state.send_tasks = state.send_tasks.saturating_sub(1);
        if state.send_tasks == 0 {
            state.send_idle.notify_waiters();
        }
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
