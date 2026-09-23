//! Live [`DiscordApi`] on `twilight-http`. Feature `discord-bot` only.
//!
//! Bot token only. Errors map to [`DiscordApiError`] and drop response bodies.

use std::fmt;

use twilight_http::Client;
use twilight_http::error::{Error, ErrorType};
use twilight_model::channel::message::Message;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::id::Id;

use super::api::{
    ApiFuture, ChannelKind, ChannelSummary, DiscordApi, DiscordApiError, GuildSummary,
    MessageSummary, Overwrite, OverwriteTarget,
};

pub(crate) struct TwilightApi {
    client: Client,
}

impl TwilightApi {
    /// Stores the token. Sends no request. Call on the tokio worker.
    pub(crate) fn new(token: String) -> Self {
        // twilight-http 0.17 does not pick a rustls crypto provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            client: Client::new(token),
        }
    }
}

impl fmt::Debug for TwilightApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TwilightApi { token: <redacted> }")
    }
}

fn id<T>(raw: u64) -> Result<Id<T>, DiscordApiError> {
    Id::new_checked(raw).ok_or(DiscordApiError::Rejected)
}

fn map_error(error: &Error) -> DiscordApiError {
    match error.kind() {
        ErrorType::Unauthorized => DiscordApiError::Unauthorized,
        ErrorType::Response { status, .. } => match status.get() {
            401 => DiscordApiError::Unauthorized,
            403 => DiscordApiError::Forbidden,
            404 => DiscordApiError::NotFound,
            429 => DiscordApiError::RateLimited,
            _ => DiscordApiError::Rejected,
        },
        ErrorType::Validation | ErrorType::BuildingRequest => DiscordApiError::Rejected,
        _ => DiscordApiError::Transport,
    }
}

fn channel_summary(channel: Channel) -> ChannelSummary {
    let kind = match channel.kind {
        ChannelType::GuildText => ChannelKind::Text,
        ChannelType::GuildAnnouncement => ChannelKind::Announcement,
        _ => ChannelKind::Other,
    };
    let overwrites = channel
        .permission_overwrites
        .unwrap_or_default()
        .into_iter()
        .filter_map(|overwrite| {
            let target = match overwrite.kind {
                PermissionOverwriteType::Role => OverwriteTarget::Role(overwrite.id.get()),
                PermissionOverwriteType::Member => OverwriteTarget::Member(overwrite.id.get()),
                _ => return None,
            };
            Some(Overwrite {
                target,
                allow: overwrite.allow.bits(),
                deny: overwrite.deny.bits(),
            })
        })
        .collect();
    ChannelSummary {
        id: channel.id.get(),
        name: channel.name.unwrap_or_default(),
        kind,
        last_message_id: channel.last_message_id.map(Id::get),
        overwrites,
    }
}

fn message_summary(message: Message) -> MessageSummary {
    let author = message.author.global_name.unwrap_or(message.author.name);
    MessageSummary {
        id: message.id.get(),
        author_id: message.author.id.get(),
        author,
        content: message.content,
        attachments: message.attachments.len(),
    }
}

impl DiscordApi for TwilightApi {
    fn bot_user_id(&self) -> ApiFuture<'_, u64> {
        Box::pin(async move {
            let response = self
                .client
                .current_user()
                .await
                .map_err(|e| map_error(&e))?;
            let user = response
                .model()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(user.id.get())
        })
    }

    fn guilds(&self) -> ApiFuture<'_, Vec<GuildSummary>> {
        Box::pin(async move {
            let response = self
                .client
                .current_user_guilds()
                .await
                .map_err(|e| map_error(&e))?;
            let guilds = response
                .models()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(guilds
                .into_iter()
                .map(|guild| GuildSummary {
                    id: guild.id.get(),
                    name: guild.name,
                    owner: guild.owner,
                    permissions: guild.permissions.bits(),
                })
                .collect())
        })
    }

    fn member_roles(&self, guild_id: u64, user_id: u64) -> ApiFuture<'_, Vec<u64>> {
        Box::pin(async move {
            let response = self
                .client
                .guild_member(id(guild_id)?, id(user_id)?)
                .await
                .map_err(|e| map_error(&e))?;
            let member = response
                .model()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(member.roles.into_iter().map(Id::get).collect())
        })
    }

    fn channels(&self, guild_id: u64) -> ApiFuture<'_, Vec<ChannelSummary>> {
        Box::pin(async move {
            let response = self
                .client
                .guild_channels(id(guild_id)?)
                .await
                .map_err(|e| map_error(&e))?;
            let channels = response
                .models()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(channels.into_iter().map(channel_summary).collect())
        })
    }

    fn history(&self, channel_id: u64, limit: u16) -> ApiFuture<'_, Vec<MessageSummary>> {
        Box::pin(async move {
            let response = self
                .client
                .channel_messages(id(channel_id)?)
                .limit(limit)
                .await
                .map_err(|e| map_error(&e))?;
            let messages = response
                .models()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(messages.into_iter().map(message_summary).collect())
        })
    }

    fn send(&self, channel_id: u64, body: String) -> ApiFuture<'_, MessageSummary> {
        Box::pin(async move {
            let response = self
                .client
                .create_message(id(channel_id)?)
                .content(&body)
                .await
                .map_err(|e| map_error(&e))?;
            let message = response
                .model()
                .await
                .map_err(|_| DiscordApiError::Transport)?;
            Ok(message_summary(message))
        })
    }
}
