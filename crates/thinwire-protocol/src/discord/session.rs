//! Live bot inbox session. Every HTTP call runs in a tokio task.
//!
//! A generation counter drops results from a session that a later connect or a
//! disconnect replaced. Only channels from the last list may open or send.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::api::DiscordApi;
use super::inbox::{HISTORY_LIMIT, InboxChannel, chat_message, load_channels};
use super::{BOT_TOKEN_PRESENT, UNKNOWN_CHANNEL_REFUSAL};
use crate::adapter::{
    AdapterError, AdapterEvent, AdapterStatus, ChatMessage, EventTx, ProtocolId, emit_conversation,
    emit_conversation_removed, emit_message, emit_message_replaced, emit_notice, emit_status,
};

const READ_ONLY_REFUSAL: &str = "The bot does not have Send Messages in that channel.";

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChannelAccess {
    channel_id: u64,
    can_send: bool,
}

#[derive(Debug, Default)]
struct Shared {
    bot_id: Option<u64>,
    channels: HashMap<String, ChannelAccess>,
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

    /// Starts a new generation and loads the channel list.
    ///
    /// `carried` is the previous list. An empty map is a first connect.
    pub(crate) fn start(
        api: Arc<dyn DiscordApi>,
        live: &Arc<AtomicU64>,
        events: &EventTx,
        carried: HashMap<String, ChannelAccess>,
    ) -> Self {
        let generation = live.fetch_add(1, Ordering::SeqCst) + 1;
        let session = Self {
            api,
            shared: Arc::new(Mutex::new(Shared {
                bot_id: None,
                channels: carried,
            })),
            gate: Gate {
                live: Arc::clone(live),
                generation,
            },
            next_pending: 0,
        };
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
            return Ok(());
        };
        let api = Arc::clone(&self.api);
        let gate = self.gate.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let result = api.history(access.channel_id, HISTORY_LIMIT).await;
            if !gate.current() {
                return;
            }
            match result {
                Ok(messages) => {
                    for message in messages.iter().rev() {
                        emit_message(&events, chat_message(&conversation_id, bot_id, message));
                    }
                }
                Err(error) => {
                    tracing::info!(%error, "discord history failed");
                    emit_ready(&events, &format!("History did not load: {error}."));
                }
            }
        });
        Ok(())
    }

    /// Sends as the bot. A pending row shows at once and is replaced on success.
    pub(crate) fn send(
        &mut self,
        conversation_id: String,
        body: String,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        let Some((access, bot_id)) = self.noted_access(&conversation_id, events)? else {
            return Ok(());
        };
        if !access.can_send {
            emit_notice(events, ProtocolId::Discord, READ_ONLY_REFUSAL);
            return Ok(());
        }
        self.next_pending += 1;
        let pending_id = format!("discord:pending:{}", self.next_pending);
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
                sent_at: 0,
            },
        );
        let api = Arc::clone(&self.api);
        let gate = self.gate.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let result = api.send(access.channel_id, body).await;
            if !gate.current() {
                return;
            }
            match result {
                Ok(sent) => {
                    emit_message_replaced(
                        &events,
                        pending_id,
                        chat_message(&conversation_id, bot_id, &sent),
                    );
                }
                Err(error) => {
                    tracing::info!(%error, "discord send failed");
                    let _ = events.send(AdapterEvent::MessagesRemoved {
                        protocol: ProtocolId::Discord,
                        conversation_id,
                        message_ids: vec![pending_id],
                    });
                    emit_ready(&events, &format!("Send failed: {error}."));
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
    for id in gone {
        emit_conversation_removed(events, ProtocolId::Discord, id);
    }
    emit_ready(
        events,
        &format!("{} guild channels the bot can read.", channels.len()),
    );
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
