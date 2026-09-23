//! Bot HTTP seam for the guild inbox. Plain types, no twilight in the trait.
//!
//! The live backend is `twilight.rs` (feature `discord-bot`). Tests use a fake.
//! No method takes or returns a token.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

/// Boxed future returned by [`DiscordApi`] calls. Runs on the tokio worker.
pub(crate) type ApiFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DiscordApiError>> + Send + 'a>>;

/// A guild the bot is in, with the bot's base permissions (no channel overwrites).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuildSummary {
    pub id: u64,
    pub name: String,
    pub owner: bool,
    pub permissions: u64,
}

/// Channel kinds the inbox can show. Threads, voice, and forums are `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelKind {
    Text,
    Announcement,
    Other,
}

/// Target of a channel permission overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverwriteTarget {
    /// Role id. The `@everyone` role id is the guild id.
    Role(u64),
    Member(u64),
}

/// One channel permission overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overwrite {
    pub target: OverwriteTarget,
    pub allow: u64,
    pub deny: u64,
}

/// A guild channel as the bot sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelSummary {
    pub id: u64,
    pub name: String,
    pub kind: ChannelKind,
    pub last_message_id: Option<u64>,
    pub overwrites: Vec<Overwrite>,
}

/// A guild channel message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MessageSummary {
    pub id: u64,
    pub author_id: u64,
    pub author: String,
    pub content: String,
    pub attachments: usize,
}

/// Discord HTTP failure. Never carries a token or a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscordApiError {
    /// 401 or a revoked token.
    Unauthorized,
    /// 403. The bot lacks a permission.
    Forbidden,
    NotFound,
    RateLimited,
    /// Discord or local validation rejected the request.
    Rejected,
    /// Network or decode failure.
    Transport,
}

impl DiscordApiError {
    #[must_use]
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Unauthorized => {
                "Discord rejected the bot token. Replace discord.bot_token in the OS keychain."
            }
            Self::Forbidden => "the bot does not have permission for that channel",
            Self::NotFound => "Discord did not find that guild or channel",
            Self::RateLimited => "Discord rate limit. Try again later",
            Self::Rejected => "Discord rejected the request",
            Self::Transport => "network error while talking to Discord",
        }
    }
}

impl fmt::Display for DiscordApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

/// Bot-token HTTP calls the guild inbox needs. No user-account endpoints.
pub(crate) trait DiscordApi: Send + Sync {
    /// Id of the bot user that owns the token.
    fn bot_user_id(&self) -> ApiFuture<'_, u64>;
    /// Guilds the bot was installed into.
    fn guilds(&self) -> ApiFuture<'_, Vec<GuildSummary>>;
    /// Role ids of the bot member in one guild.
    fn member_roles(&self, guild_id: u64, user_id: u64) -> ApiFuture<'_, Vec<u64>>;
    /// Channels of one guild.
    fn channels(&self, guild_id: u64) -> ApiFuture<'_, Vec<ChannelSummary>>;
    /// Recent messages, newest first (Discord order).
    fn history(&self, channel_id: u64, limit: u16) -> ApiFuture<'_, Vec<MessageSummary>>;
    /// Send plain text as the bot.
    fn send(&self, channel_id: u64, body: String) -> ApiFuture<'_, MessageSummary>;
}

#[cfg(test)]
mod tests {
    use super::DiscordApiError;

    #[test]
    fn error_reasons_are_short_and_carry_no_credentials() {
        for error in [
            DiscordApiError::Unauthorized,
            DiscordApiError::Forbidden,
            DiscordApiError::NotFound,
            DiscordApiError::RateLimited,
            DiscordApiError::Rejected,
            DiscordApiError::Transport,
        ] {
            let text = error.to_string();
            assert!(!text.is_empty());
            assert!(!text.to_ascii_lowercase().contains("bearer"));
            assert!(!text.to_ascii_lowercase().contains("reliable"));
        }
        assert!(
            DiscordApiError::Unauthorized
                .reason()
                .contains("discord.bot_token")
        );
    }
}
