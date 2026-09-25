//! Live bot inbox session. Every HTTP call runs in a tokio task.
//!
//! A generation counter drops results from a session that a later connect or a
//! disconnect replaced. Only channels from the last list may open or send.

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

#[derive(Debug, Default)]
struct Shared {
    bot_id: Option<u64>,
    channels: HashMap<String, ChannelAccess>,
    inflight: Vec<Inflight>,
    /// History loads still running. A 401 finishes them before `Unlinked`.
    loads: Vec<PendingLoad>,
    /// Message ids from the last history page for each channel.
    history: HashMap<String, Vec<String>>,
    /// Body of each outgoing row, so a retry can post it again.
    bodies: HashMap<String, String>,
    /// Latest `reload` in this session. An older list must not publish.
    reload_ticket: u64,
    /// A 401 revoked the token. The adapter drops this session on the next command.
    revoked: bool,
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
    pub(crate) fn start(
        api: Arc<dyn DiscordApi>,
        live: &Arc<AtomicU64>,
        events: &EventTx,
        carried: HashMap<String, ChannelAccess>,
        history: HashMap<String, Vec<String>>,
        bodies: HashMap<String, String>,
    ) -> Self {
        let generation = live.fetch_add(1, Ordering::SeqCst) + 1;
        let session = Self {
            api,
            shared: Arc::new(Mutex::new(Shared {
                bot_id: None,
                channels: carried,
                inflight: Vec::new(),
                loads: Vec::new(),
                history,
                bodies,
                reload_ticket: 0,
                revoked: false,
                send_tasks: 0,
                send_idle: Arc::new(Notify::new()),
            })),
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
            state.reload_ticket = state.reload_ticket.wrapping_add(1);
            state.reload_ticket
        };
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let result = load_channels(api.as_ref()).await;
            if !gate.current() {
                return;
            }
            let current = shared.lock().map(|state| state.reload_ticket).unwrap_or(0);
            if current != ticket {
                return;
            }
            match result {
                Ok((bot_id, channels)) => {
                    publish_channels(&shared, &events, bot_id, &channels);
                    // The shell stops the chat-list spinner on this event
                    // (adapter contract rule 9, ADR 0010).
                    emit_chat_list_loaded(&events, ProtocolId::Discord);
                }
                Err(error) => {
                    tracing::info!(%error, "discord channel list failed");
                    if error == DiscordApiError::Unauthorized {
                        unlink_unauthorized(&shared, &gate, &events, error);
                    } else {
                        emit_command_failed(
                            &events,
                            ProtocolId::Discord,
                            None,
                            format!("Discord bot inbox did not load: {error}"),
                        );
                    }
                    emit_status(
                        &events,
                        ProtocolId::Discord,
                        AdapterStatus::Error,
                        format!("Discord bot inbox did not load: {error}"),
                    );
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
        let Some((access, bot_id)) = self.noted_access(&conversation_id, events)? else {
            emit_command_failed(
                events,
                ProtocolId::Discord,
                Some(conversation_id.clone()),
                UNKNOWN_CHANNEL_REFUSAL,
            );
            emit_history_loaded(events, ProtocolId::Discord, conversation_id);
            return Ok(());
        };
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let events = events.clone();
        if let Ok(mut state) = self.shared.lock() {
            state.loads.push(PendingLoad {
                conversation_id: conversation_id.clone(),
            });
        }
        tokio::spawn(async move {
            let result = api.history(access.channel_id, HISTORY_LIMIT).await;
            // A newer connect replaced this load. Finish it so the shell drops
            // the spinner. A 401 already settled this load before Unlinked.
            if !gate.current() {
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
                    let gone = {
                        let Ok(mut state) = shared.lock() else {
                            drop_load(&shared, &conversation_id);
                            emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                            return;
                        };
                        if !gate.current() {
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
                        gone
                    };
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
                    if error == DiscordApiError::Unauthorized {
                        unlink_unauthorized(&shared, &gate, &events, error);
                        return;
                    }
                    drop_load(&shared, &conversation_id);
                    emit_command_failed(
                        &events,
                        ProtocolId::Discord,
                        Some(conversation_id.clone()),
                        format!("History did not load: {error}."),
                    );
                    emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
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
        let Some((access, bot_id)) = self.noted_access(&conversation_id, events)? else {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            return Ok(());
        };
        if !access.can_send {
            emit_send_rejected(
                events,
                ProtocolId::Discord,
                conversation_id.as_str(),
                request,
            );
            emit_notice(events, ProtocolId::Discord, READ_ONLY_REFUSAL);
            return Ok(());
        }
        self.next_pending += 1;
        let pending_id = format!(
            "discord:pending:{}:{}",
            self.gate.generation, self.next_pending
        );
        if let Ok(mut state) = self.shared.lock() {
            state.inflight.push(Inflight {
                conversation_id: conversation_id.clone(),
                request,
            });
            state.bodies.insert(pending_id.clone(), body.clone());
        }
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: conversation_id.clone(),
                id: pending_id.clone(),
                sender: "bot".into(),
                body: body.clone(),
                outbound: true,
                delivery: crate::adapter::Delivery::Pending,
                sent_at: local_unix_seconds(),
            },
        );
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
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
                    message_id: pending_id,
                    request,
                    bot_id,
                    row: SendRow::Pending,
                    result,
                },
            )
            .await;
        });
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
        let Some((access, bot_id)) = self.noted_access(&conversation_id, events)? else {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            return Ok(());
        };
        if !access.can_send {
            emit_send_rejected(
                events,
                ProtocolId::Discord,
                conversation_id.as_str(),
                request,
            );
            emit_notice(events, ProtocolId::Discord, READ_ONLY_REFUSAL);
            return Ok(());
        }
        let body = self
            .shared
            .lock()
            .ok()
            .and_then(|state| state.bodies.get(&message_id).cloned());
        let Some(body) = body else {
            emit_send_rejected(events, ProtocolId::Discord, conversation_id, request);
            return Ok(());
        };
        if let Ok(mut state) = self.shared.lock() {
            state.inflight.push(Inflight {
                conversation_id: conversation_id.clone(),
                request,
            });
        }
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
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
                    row: SendRow::Retry,
                    result,
                },
            )
            .await;
        });
        Ok(())
    }

    /// `Ok(None)` means the refusal is a note. The account status stays Ready.
    fn noted_access(
        &self,
        conversation_id: &str,
        events: &EventTx,
    ) -> Result<Option<(ChannelAccess, u64)>, AdapterError> {
        match self.access(conversation_id) {
            Ok(pair) => Ok(Some(pair)),
            Err(AdapterError::Refused { reason, .. }) => {
                emit_notice(events, ProtocolId::Discord, reason);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn access(&self, conversation_id: &str) -> Result<(ChannelAccess, u64), AdapterError> {
        let refused = AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: UNKNOWN_CHANNEL_REFUSAL,
        };
        let Ok(shared) = self.shared.lock() else {
            return Err(refused);
        };
        match (shared.channels.get(conversation_id), shared.bot_id) {
            (Some(access), Some(bot_id)) => Ok((*access, bot_id)),
            (Some(_), None) => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Discord,
                reason: "Discord bot inbox is still loading guild channels.",
            }),
            _ => Err(refused),
        }
    }
}

fn publish_channels(
    shared: &Mutex<Shared>,
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
    let gone: Vec<String> = {
        let Ok(mut state) = shared.lock() else {
            return;
        };
        let gone = state
            .channels
            .keys()
            .filter(|id| !next.contains_key(*id))
            .cloned()
            .collect();
        state.bot_id = Some(bot_id);
        state.channels = next;
        gone
    };
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
}

/// A 401 means the token is dead. Reject every send still in flight and
/// finish every history load, then unlink. The result events are queued
/// before the lock is released, so a send task cannot publish a second one
/// after `Unlinked` (ADR 0010 drops those).
fn unlink_unauthorized(
    shared: &Arc<Mutex<Shared>>,
    gate: &Gate,
    events: &EventTx,
    error: DiscordApiError,
) {
    let Ok(mut state) = shared.lock() else {
        emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
        emit_notice(events, ProtocolId::Discord, error.reason());
        gate.invalidate();
        return;
    };
    if state.revoked {
        return;
    }
    settle_unauthorized(&mut state, gate, events, error);
}

/// Caller holds the session lock. Queues every still-tracked result, then
/// `Unlinked`, before that lock is released.
fn settle_unauthorized(state: &mut Shared, gate: &Gate, events: &EventTx, error: DiscordApiError) {
    state.revoked = true;
    let sends = std::mem::take(&mut state.inflight);
    let loads = std::mem::take(&mut state.loads);
    for send in sends {
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
    emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
    emit_notice(events, ProtocolId::Discord, error.reason());
    gate.invalidate();
}

fn session_revoked(shared: &Arc<Mutex<Shared>>) -> bool {
    shared.lock().map(|state| state.revoked).unwrap_or(true)
}

/// Optimistic row (`Pending`) or a retry of a row the shell already has.
enum SendRow {
    Pending,
    Retry,
}

struct ReturnedSend {
    conversation_id: String,
    message_id: String,
    request: u64,
    bot_id: u64,
    row: SendRow,
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
    let queued = queue_send_result(shared, gate, events, returned);
    if queued {
        // The result is already on the channel. A test can run a 401 here.
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
        result,
    } = returned;
    let mine = |send: &Inflight| send.request == request && send.conversation_id == conversation_id;
    if state.revoked || !state.inflight.iter().any(mine) {
        return false;
    }
    state.inflight.retain(|send| !mine(send));
    if !gate.current() {
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
                settle_unauthorized(&mut state, gate, events, error);
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
