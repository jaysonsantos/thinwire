//! Live [`DiscordApi`] on `twilight-http`. Feature `discord-bot` only.
//!
//! Bot token only. Errors map to [`DiscordApiError`] and drop response bodies.

use std::fmt;
use std::time::Duration;

use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::{Error, ErrorType};
use twilight_model::channel::message::Message;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::id::Id;

use super::api::{
    ApiFuture, ChannelKind, ChannelSummary, DiscordApi, DiscordApiError, GuildSummary,
    MessageSummary, Overwrite, OverwriteTarget, RATE_LIMIT_FALLBACK,
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

    fn guild_page(&self, after: Option<u64>) -> ApiFuture<'_, Vec<GuildSummary>> {
        Box::pin(async move {
            let mut request = self
                .client
                .current_user_guilds()
                .limit(super::api::GUILD_PAGE_LIMIT);
            if let Some(raw) = after {
                request = request.after(id(raw)?);
            }
            let response = request.await.map_err(|e| map_error(&e))?;
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
        ErrorType::Response {
            status,
            error: api_error,
            ..
        } => match status.get() {
            401 => DiscordApiError::Unauthorized,
            403 => DiscordApiError::Forbidden,
            404 => DiscordApiError::NotFound,
            429 => DiscordApiError::RateLimited {
                retry_after: retry_after_of(api_error),
            },
            _ => DiscordApiError::Rejected,
        },
        ErrorType::Validation | ErrorType::BuildingRequest => DiscordApiError::Rejected,
        _ => DiscordApiError::Transport,
    }
}

fn retry_after_of(error: &ApiError) -> Duration {
    let ApiError::Ratelimited(body) = error else {
        return RATE_LIMIT_FALLBACK;
    };
    let seconds = body.retry_after;
    if !seconds.is_finite() || seconds <= 0.0 {
        return RATE_LIMIT_FALLBACK;
    }
    Duration::try_from_secs_f64(seconds.min(30.0)).unwrap_or(RATE_LIMIT_FALLBACK)
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
    let mut images = 0;
    let mut files = 0;
    for attachment in &message.attachments {
        if attachment
            .content_type
            .as_deref()
            .is_some_and(|kind| kind.starts_with("image/"))
        {
            images += 1;
        } else {
            files += 1;
        }
    }
    MessageSummary {
        id: message.id.get(),
        author_id: message.author.id.get(),
        author,
        content: message.content,
        images,
        files,
        embeds: message.embeds.len(),
        stickers: message.sticker_items.len(),
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
        Box::pin(
            async move { super::api::collect_guild_pages(|after| self.guild_page(after)).await },
        )
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
