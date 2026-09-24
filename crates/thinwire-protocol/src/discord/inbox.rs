//! Guild inbox logic over [`DiscordApi`]: channel list and message mapping.
//!
//! Only guild text and announcement channels that the bot can read are listed.
//! Direct-message channels never come from these calls.

use super::api::{ChannelKind, DiscordApi, DiscordApiError, MessageSummary};
use super::permissions::{MemberBase, can_read, can_send, channel_permissions};
use crate::adapter::{ChatMessage, Conversation, Delivery, ProtocolId};

/// Messages loaded when a channel opens. Discord allows 1..=100.
pub(crate) const HISTORY_LIMIT: u16 = 50;

const CONVERSATION_PREFIX: &str = "discord:";

/// A guild channel the bot can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboxChannel {
    pub guild_id: u64,
    pub guild_name: String,
    pub channel_id: u64,
    pub name: String,
    pub last_message_id: Option<u64>,
    pub can_send: bool,
}

impl InboxChannel {
    #[must_use]
    pub(crate) fn conversation_id(&self) -> String {
        conversation_id(self.guild_id, self.channel_id)
    }

    #[must_use]
    pub(crate) fn conversation(&self) -> Conversation {
        let preview = if self.can_send {
            "Guild channel. The bot can read and send."
        } else {
            "Guild channel. The bot can read only."
        };
        Conversation {
            protocol: ProtocolId::Discord,
            id: self.conversation_id(),
            title: format!("#{}", self.name),
            participant: self.guild_name.clone(),
            preview: preview.into(),
            unread: 0,
            // Snowflakes grow with time, so the newest channel sorts first.
            order: self
                .last_message_id
                .and_then(|id| i64::try_from(id).ok())
                .unwrap_or(0),
            last_at: 0,
            is_group: true,
            writable: self.can_send,
            placeholder: false,
        }
    }
}

/// `discord:<guild_id>:<channel_id>`. Not a secret.
#[must_use]
pub(crate) fn conversation_id(guild_id: u64, channel_id: u64) -> String {
    format!("{CONVERSATION_PREFIX}{guild_id}:{channel_id}")
}

/// Bot user id and every readable guild channel.
///
/// A guild that answers 403 or 404 is skipped. A bad token stops the load.
pub(crate) async fn load_channels(
    api: &dyn DiscordApi,
) -> Result<(u64, Vec<InboxChannel>), DiscordApiError> {
    let bot_id = api.bot_user_id().await?;
    let mut list = Vec::new();
    for guild in api.guilds().await? {
        let roles = match api.member_roles(guild.id, bot_id).await {
            Ok(roles) => roles,
            Err(DiscordApiError::Forbidden | DiscordApiError::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let channels = match api.channels(guild.id).await {
            Ok(channels) => channels,
            Err(DiscordApiError::Forbidden | DiscordApiError::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let member = MemberBase {
            guild_id: guild.id,
            member_id: bot_id,
            owner: guild.owner,
            base: guild.permissions,
            roles: &roles,
        };
        for channel in channels {
            if !matches!(channel.kind, ChannelKind::Text | ChannelKind::Announcement) {
                continue;
            }
            let perms = channel_permissions(member, &channel.overwrites);
            if !can_read(perms) {
                continue;
            }
            list.push(InboxChannel {
                guild_id: guild.id,
                guild_name: guild.name.clone(),
                channel_id: channel.id,
                name: channel.name,
                last_message_id: channel.last_message_id,
                can_send: can_send(perms),
            });
        }
    }
    Ok((bot_id, list))
}

/// Message row for the right pane. `outbound` marks the bot's own messages.
#[must_use]
pub(crate) fn chat_message(
    conversation_id: &str,
    bot_id: u64,
    message: &MessageSummary,
) -> ChatMessage {
    let body = if !message.content.is_empty() {
        message.content.clone()
    } else if message.attachments > 0 {
        "[attachment]".into()
    } else {
        "[no text]".into()
    };
    ChatMessage {
        protocol: ProtocolId::Discord,
        conversation_id: conversation_id.into(),
        id: format!("{CONVERSATION_PREFIX}{}", message.id),
        sender: message.author.clone(),
        body,
        outbound: message.author_id == bot_id,
        delivery: Delivery::Sent,
        sent_at: 0,
    }
}
