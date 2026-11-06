//! Guild inbox logic over [`DiscordApi`]: channel list and message mapping.
//!
//! Only guild text and announcement channels that the bot can read are listed.
//! Direct-message channels never come from these calls.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::api::{ChannelKind, DiscordApi, DiscordApiError, MessageSummary};
use super::permissions::{MemberBase, can_read, can_send, channel_permissions};
use crate::adapter::{ChatMessage, Conversation, Delivery, ProtocolId};

/// Messages loaded when a channel opens. Discord allows 1..=100.
pub(crate) const HISTORY_LIMIT: u16 = 50;

/// Preview history calls in flight at once. The list is published before any of them.
pub(crate) const PREVIEW_FETCH_CONCURRENCY: usize = 4;

/// Give up on a preview after this many 429s. The row stays empty.
const PREVIEW_RATE_LIMIT_ATTEMPTS: u32 = 3;

pub(crate) const CONVERSATION_PREFIX: &str = "discord:";

/// A guild channel the bot can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboxChannel {
    pub guild_id: u64,
    pub guild_name: String,
    pub channel_id: u64,
    pub name: String,
    pub last_message_id: Option<u64>,
    pub can_send: bool,
    /// Latest message text, or empty when the channel has no visible message.
    pub preview: String,
}

impl InboxChannel {
    #[must_use]
    pub(crate) fn conversation_id(&self) -> String {
        conversation_id(self.guild_id, self.channel_id)
    }

    #[must_use]
    pub(crate) fn conversation(&self) -> Conversation {
        Conversation {
            protocol: ProtocolId::Discord,
            id: self.conversation_id(),
            title: format!("#{}", self.name),
            participant: self.guild_name.clone(),
            preview: self.preview.clone(),
            unread: 0,
            // Snowflakes grow with time, so the newest channel sorts first.
            order: self
                .last_message_id
                .and_then(|id| i64::try_from(id).ok())
                .unwrap_or(0),
            last_at: self
                .last_message_id
                .map(snowflake_unix_seconds)
                .unwrap_or(0),
            is_group: true,
            writable: self.can_send,
            muted: false,
            placeholder: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }
}

/// `discord:<guild_id>:<channel_id>`. Not a secret.
#[must_use]
pub(crate) fn conversation_id(guild_id: u64, channel_id: u64) -> String {
    format!("{CONVERSATION_PREFIX}{guild_id}:{channel_id}")
}

/// Bot user id and every readable guild channel, with empty previews.
///
/// A guild that answers 403 or 404 is skipped. A bad token stops the load.
/// Previews are filled later so linking does not wait on one history call per channel.
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
                preview: String::new(),
            });
        }
    }
    Ok((bot_id, list))
}

/// Newest visible message, or empty. A per-channel failure stays empty.
/// `Unauthorized` is returned so the session can unlink, as it does for history.
pub(crate) async fn channel_preview(
    api: &dyn DiscordApi,
    channel_id: u64,
    pause: &PreviewPause,
) -> Result<String, DiscordApiError> {
    for attempt in 0..PREVIEW_RATE_LIMIT_ATTEMPTS {
        pause.wait().await;
        match api.history(channel_id, 1).await {
            Ok(latest) => return Ok(preview_text(latest.first())),
            Err(DiscordApiError::Unauthorized) => return Err(DiscordApiError::Unauthorized),
            Err(DiscordApiError::RateLimited { retry_after }) => {
                pause.push(retry_after);
                if attempt + 1 == PREVIEW_RATE_LIMIT_ATTEMPTS {
                    tracing::info!(
                        reason = DiscordApiError::RateLimited { retry_after }.reason(),
                        "discord channel preview did not load"
                    );
                    return Ok(String::new());
                }
            }
            Err(error) => {
                tracing::info!(
                    reason = error.reason(),
                    "discord channel preview did not load"
                );
                return Ok(String::new());
            }
        }
    }
    Ok(String::new())
}

fn preview_text(latest: Option<&MessageSummary>) -> String {
    latest
        .map(message_body)
        .filter(|text| text != "[no text]")
        .unwrap_or_default()
}

/// Shared pause so one 429 holds the other preview calls too.
pub(crate) struct PreviewPause {
    until: Mutex<Instant>,
}

impl PreviewPause {
    pub(crate) fn new() -> Self {
        Self {
            until: Mutex::new(Instant::now()),
        }
    }

    async fn wait(&self) {
        loop {
            let delay = self
                .until
                .lock()
                .map(|until| until.saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            if delay.is_zero() {
                return;
            }
            tokio::time::sleep(delay).await;
        }
    }

    fn push(&self, retry_after: Duration) {
        let Ok(mut until) = self.until.lock() else {
            return;
        };
        let next = Instant::now() + retry_after;
        if next > *until {
            *until = next;
        }
    }
}

/// The body of a message that has only an image.
pub(crate) const IMAGE_LABEL: &str = "[image]";

/// Visible text. Discord's REST `content` is kept as returned (the Message
/// Content intent fills it for other people). A message with no content is
/// labeled from what it does carry.
#[must_use]
pub(crate) fn message_body(message: &MessageSummary) -> String {
    if !message.content.is_empty() {
        return message.content.clone();
    }
    let mut labels = Vec::new();
    if message.images > 0 {
        labels.push(IMAGE_LABEL);
    }
    if let Some(audio) = &message.audio {
        labels.push(if audio.voice {
            "[voice message]"
        } else {
            "[audio]"
        });
    }
    // The audio attachment is one of the files.
    if message.files > usize::from(message.audio.is_some()) {
        labels.push("[attachment]");
    }
    if message.embeds > 0 {
        labels.push("[embed]");
    }
    if message.stickers > 0 {
        labels.push("[sticker]");
    }
    if labels.is_empty() {
        "[no text]".into()
    } else {
        labels.join(" ")
    }
}

/// Message row for the right pane. `outbound` marks the bot's own messages.
#[must_use]
pub(crate) fn chat_message(
    conversation_id: &str,
    bot_id: u64,
    message: &MessageSummary,
) -> ChatMessage {
    ChatMessage {
        protocol: ProtocolId::Discord,
        conversation_id: conversation_id.into(),
        id: format!("{CONVERSATION_PREFIX}{}", message.id),
        sender: message.author.clone(),
        body: message_body(message),
        outbound: message.author_id == bot_id,
        delivery: Delivery::Sent,
        sent_at: snowflake_unix_seconds(message.id),
        arrival: crate::Arrival::History,
        topic: None,
        links: Vec::new(),
        photo: None,
        audio: message.audio.as_ref().and_then(|audio| {
            crate::MessageAudio::new(audio.duration_secs, voice_file_id(message.id), None)
        }),
        video: None,
        file: None,
        reactions: message.reactions.clone(),
        reply_to: message
            .reply_to
            .map(|id| format!("{CONVERSATION_PREFIX}{id}")),
    }
}

/// The snowflake of a Discord row id.
#[must_use]
pub(crate) fn message_snowflake(message_id: &str) -> Option<u64> {
    message_id.strip_prefix(CONVERSATION_PREFIX)?.parse().ok()
}

/// The handle of the audio of one message for `DownloadVoice`. The same
/// message has the same handle in every run. Always positive.
#[must_use]
pub(crate) fn voice_file_id(message_id: u64) -> i32 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(message_id);
    let low = u32::try_from(hash & 0x7fff_ffff).unwrap_or(1);
    i32::try_from(low).unwrap_or(1).max(1)
}

/// Discord snowflake epoch, 2015-01-01T00:00:00Z, in milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Unix seconds in a Discord snowflake.
#[must_use]
pub(crate) fn snowflake_unix_seconds(id: u64) -> i64 {
    let ms = (id >> 22).saturating_add(DISCORD_EPOCH_MS);
    i64::try_from(ms / 1_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::api::MessageSummary;
    use super::{chat_message, snowflake_unix_seconds};

    #[test]
    fn a_snowflake_keeps_unix_seconds() {
        let ms = 1_700_000_000_000_u64;
        let id = (ms - 1_420_070_400_000) << 22;
        assert_eq!(snowflake_unix_seconds(id), 1_700_000_000);
        let row = chat_message(
            "discord:1:2",
            9,
            &MessageSummary {
                id,
                author_id: 3,
                author: "ada".into(),
                content: "hi".into(),
                images: 0,
                files: 0,
                embeds: 0,
                stickers: 0,
                audio: None,
                image: None,
                reactions: Vec::new(),
                reply_to: None,
            },
        );
        assert_eq!(row.sent_at, 1_700_000_000);
        // The HTTP inbox has no gateway: every message comes from a history
        // load, so none is live and Discord does not notify yet (#32).
        assert_eq!(row.arrival, crate::Arrival::History);
        for src in [
            include_str!("inbox.rs"),
            include_str!("session.rs"),
            include_str!("mod.rs"),
        ] {
            assert!(
                !src.contains(concat!("Arrival::", "Live")),
                "a gateway path must mark its new messages live"
            );
        }
    }

    #[test]
    fn a_message_keeps_text_and_labels_media() {
        use super::message_body;

        let mut row = MessageSummary {
            id: 1,
            author_id: 2,
            author: "ada".into(),
            content: "hello".into(),
            images: 1,
            files: 0,
            embeds: 0,
            stickers: 0,
            audio: None,
            image: None,
            reactions: Vec::new(),
            reply_to: None,
        };
        assert_eq!(message_body(&row), "hello");
        row.content.clear();
        assert_eq!(message_body(&row), "[image]");
        row.images = 0;
        row.files = 1;
        assert_eq!(message_body(&row), "[attachment]");
        row.files = 0;
        row.embeds = 1;
        assert_eq!(message_body(&row), "[embed]");
        row.embeds = 0;
        row.stickers = 1;
        assert_eq!(message_body(&row), "[sticker]");
        row.stickers = 0;
        assert_eq!(message_body(&row), "[no text]");
    }
}
