//! Live bot inbox session. Every HTTP call runs in a tokio task.
//!
//! A generation counter drops results from a session that a later connect or a
//! disconnect replaced. Only channels from the last list may open or send.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::api::{DiscordApi, DiscordApiError};
use super::inbox::{HISTORY_LIMIT, InboxChannel, chat_message, load_channels};
use super::{BOT_TOKEN_PRESENT, UNKNOWN_CHANNEL_REFUSAL};
use crate::adapter::{
    AccountState, AdapterError, AdapterEvent, AdapterStatus, ChatMessage, Delivery, EventTx,
    ProtocolId, emit_account, emit_command_failed, emit_conversation, emit_conversation_removed,
    emit_history_loaded, emit_message, emit_message_delivery, emit_message_replaced, emit_notice,
    emit_send_accepted, emit_send_rejected, emit_status,
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

#[derive(Debug, Default)]
struct Shared {
    bot_id: Option<u64>,
    channels: HashMap<String, ChannelAccess>,
    inflight: Vec<Inflight>,
    /// Message ids from the last history page for each channel.
    history: HashMap<String, Vec<String>>,
    /// Body of each outgoing row, so a retry can post it again.
    bodies: HashMap<String, String>,
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

    /// Starts a new generation and loads the channel list.
    ///
    /// `carried` is the previous list. An empty map is a first connect.
    pub(crate) fn start(
        api: Arc<dyn DiscordApi>,
        live: &Arc<AtomicU64>,
        events: &EventTx,
        carried: HashMap<String, ChannelAccess>,
        history: HashMap<String, Vec<String>>,
    ) -> Self {
        let generation = live.fetch_add(1, Ordering::SeqCst) + 1;
        let session = Self {
            api,
            shared: Arc::new(Mutex::new(Shared {
                bot_id: None,
                channels: carried,
                inflight: Vec::new(),
                history,
                bodies: HashMap::new(),
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
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let result = load_channels(api.as_ref()).await;
            if !gate.current() {
                return;
            }
            match result {
                Ok((bot_id, channels)) => publish_channels(&shared, &events, bot_id, &channels),
                Err(error) => {
                    tracing::info!(%error, "discord channel list failed");
                    if error == DiscordApiError::Unauthorized {
                        emit_account(&events, ProtocolId::Discord, AccountState::Unlinked);
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
        tokio::spawn(async move {
            let result = api.history(access.channel_id, HISTORY_LIMIT).await;
            // A newer connect replaced this load. Finish it so the shell drops
            // the spinner. The new session's own open emits its own event.
            if !gate.current() {
                emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
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
                            emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                            return;
                        };
                        if !gate.current() {
                            emit_history_loaded(&events, ProtocolId::Discord, conversation_id);
                            return;
                        }
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
        let pending_id = format!("discord:pending:{}", self.next_pending);
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
        tokio::spawn(async move {
            let result = api.send(access.channel_id, body).await;
            if let Ok(mut state) = shared.lock() {
                state
                    .inflight
                    .retain(|row| row.request != request || row.conversation_id != conversation_id);
            }
            if !gate.current() {
                let _ = events.send(AdapterEvent::MessagesRemoved {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id.clone(),
                    message_ids: vec![pending_id],
                });
                emit_send_rejected(&events, ProtocolId::Discord, conversation_id, request);
                return;
            }
            match result {
                Ok(sent) => {
                    let row = chat_message(&conversation_id, bot_id, &sent);
                    if let Ok(mut state) = shared.lock() {
                        state
                            .history
                            .entry(conversation_id.clone())
                            .or_default()
                            .push(row.id.clone());
                    }
                    emit_send_accepted(
                        &events,
                        ProtocolId::Discord,
                        conversation_id.as_str(),
                        request,
                    );
                    emit_message_replaced(&events, pending_id, row);
                }
                Err(error) => {
                    tracing::info!(%error, "discord send failed");
                    emit_message_delivery(
                        &events,
                        ProtocolId::Discord,
                        conversation_id.clone(),
                        pending_id,
                        Delivery::Failed,
                    );
                    emit_send_rejected(&events, ProtocolId::Discord, conversation_id, request);
                    emit_ready(&events, &format!("Send failed: {error}."));
                }
            }
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
        let api = Arc::clone(&self.api);
        let shared = Arc::clone(&self.shared);
        let gate = self.gate.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let result = api.send(access.channel_id, body).await;
            if !gate.current() {
                emit_message_delivery(
                    &events,
                    ProtocolId::Discord,
                    conversation_id.clone(),
                    message_id,
                    Delivery::Failed,
                );
                emit_send_rejected(&events, ProtocolId::Discord, conversation_id, request);
                return;
            }
            match result {
                Ok(sent) => {
                    let row = chat_message(&conversation_id, bot_id, &sent);
                    if let Ok(mut state) = shared.lock() {
                        state
                            .history
                            .entry(conversation_id.clone())
                            .or_default()
                            .push(row.id.clone());
                    }
                    emit_send_accepted(
                        &events,
                        ProtocolId::Discord,
                        conversation_id.as_str(),
                        request,
                    );
                    emit_message_replaced(&events, message_id, row);
                }
                Err(error) => {
                    tracing::info!(%error, "discord resend failed");
                    emit_message_delivery(
                        &events,
                        ProtocolId::Discord,
                        conversation_id.clone(),
                        message_id,
                        Delivery::Failed,
                    );
                    emit_send_rejected(&events, ProtocolId::Discord, conversation_id, request);
                }
            }
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

fn emit_ready(events: &EventTx, note: &str) {
    emit_status(
        events,
        ProtocolId::Discord,
        AdapterStatus::Ready,
        format!("Discord bot inbox. {note} {BOT_TOKEN_PRESENT}. Not a personal Discord client."),
    );
}
