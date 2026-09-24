//! Slack Web API and Socket Mode seams for the workspace-app inbox.
//!
//! The session actor talks only to these traits. The live build implements
//! them with `slack-morphism` (`live.rs`, feature `slack-oauth`). Tests use
//! in-process fakes. Tokens have a redacted `Debug` and never reach an event.

use std::fmt;
use std::future::Future;

use tokio::sync::mpsc::UnboundedSender;

use super::install::SlackInstalledWorkspace;

/// Workspace bot token (`xoxb-`). `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackBotToken(String);

impl SlackBotToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Worker-only access. Callers must not log the returned string.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SlackBotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackBotToken(<redacted>)")
    }
}

/// Socket Mode app-level token (`xapp-`). `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackAppToken(String);

impl SlackAppToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Worker-only access. Callers must not log the returned string.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SlackAppToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackAppToken(<redacted>)")
    }
}

/// Values the `oauth.v2.access` exchange needs. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackCodeExchange {
    pub client_id: String,
    pub client_secret: String,
    pub code: String,
}

impl fmt::Debug for SlackCodeExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackCodeExchange(<redacted>)")
    }
}

/// Result of a workspace install. The token is a bot token, never a user token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackInstallGrant {
    pub workspace: SlackInstalledWorkspace,
    pub bot_token: SlackBotToken,
}

/// Conversation kinds a workspace bot can read with the ADR 0008 scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackChannelKind {
    Public,
    Private,
    DirectMessage,
    GroupMessage,
}

/// One row of `conversations.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackChannel {
    pub id: String,
    /// Channel name without `#`. For a DM this is empty; `dm_user` names the peer.
    pub name: String,
    pub kind: SlackChannelKind,
    /// The bot is in the channel and can read its history.
    pub is_member: bool,
    /// Peer user id for a direct message.
    pub dm_user: Option<String>,
}

/// One page of `conversations.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlackChannelPage {
    pub channels: Vec<SlackChannel>,
    /// Empty or `None` means the last page.
    pub next_cursor: Option<String>,
}

/// One message from history, `chat.postMessage`, or a Socket Mode event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPost {
    pub channel: String,
    pub ts: String,
    pub user: Option<String>,
    /// Display name Slack sent with the message (bots, integrations).
    pub username: Option<String>,
    pub text: String,
}

/// Recoverable Slack failure. Display never includes tokens or message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackApiError {
    /// Slack answered `ok: false` with this error code (for example `not_in_channel`).
    Api(String),
    RateLimited,
    Network,
}

impl SlackApiError {
    /// Keep only a short `snake_case` Slack error code. Anything else is dropped.
    #[must_use]
    pub fn api(code: &str) -> Self {
        let safe = code.len() <= 64
            && !code.is_empty()
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit());
        Self::Api(if safe {
            code.to_string()
        } else {
            "unknown_error".into()
        })
    }
}

impl fmt::Display for SlackApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(code) => write!(f, "Slack returned {code}"),
            Self::RateLimited => f.write_str("Slack rate limit; try again soon"),
            Self::Network => f.write_str("could not reach Slack"),
        }
    }
}

impl std::error::Error for SlackApiError {}

/// Slack Web API calls the inbox needs. Implementations run on the tokio worker.
pub trait SlackWebApi: Send + Sync + 'static {
    /// `auth.test`: team and bot user for a stored bot token. App id may be empty.
    fn identify(
        &self,
        token: &SlackBotToken,
    ) -> impl Future<Output = Result<SlackInstalledWorkspace, SlackApiError>> + Send;

    fn exchange_code(
        &self,
        exchange: SlackCodeExchange,
    ) -> impl Future<Output = Result<SlackInstallGrant, SlackApiError>> + Send;

    fn list_channels(
        &self,
        token: &SlackBotToken,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<SlackChannelPage, SlackApiError>> + Send;

    /// Newest first, as Slack returns it.
    fn history(
        &self,
        token: &SlackBotToken,
        channel: &str,
        limit: u16,
    ) -> impl Future<Output = Result<Vec<SlackPost>, SlackApiError>> + Send;

    fn post_message(
        &self,
        token: &SlackBotToken,
        channel: &str,
        text: &str,
    ) -> impl Future<Output = Result<SlackPost, SlackApiError>> + Send;

    /// Display name for a user id (`users.info`).
    fn user_name(
        &self,
        token: &SlackBotToken,
        user: &str,
    ) -> impl Future<Output = Result<String, SlackApiError>> + Send;
}

/// Live event pushed from Socket Mode into the session actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackInbound {
    Message(SlackPost),
    /// `message_changed`: replace the body of an existing row.
    Edited {
        channel: String,
        ts: String,
        text: String,
    },
    /// `message_deleted`: drop the row.
    Deleted {
        channel: String,
        ts: String,
    },
    /// The app was uninstalled or the token was revoked.
    Revoked,
}

/// Running Socket Mode connection. Dropping it must stop the connection.
pub trait SlackEventStream: Send + 'static {
    fn stop(self) -> impl Future<Output = ()> + Send;
}

/// Opens Socket Mode with the app-level token. Never on the UI thread.
pub trait SlackEventSource: Send + Sync + 'static {
    type Stream: SlackEventStream;

    fn connect(
        &self,
        app_token: SlackAppToken,
        sink: UnboundedSender<SlackInbound>,
    ) -> impl Future<Output = Result<Self::Stream, SlackApiError>> + Send;
}

/// Opens the workspace install page. The live build uses the system browser.
pub trait SlackBrowser: Send + Sync + 'static {
    /// Returns false when no browser could be opened. Must not log `url`.
    fn open(&self, url: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_exchange_debug_are_redacted() {
        let bot = SlackBotToken::new("xoxb-test-bot");
        let app = SlackAppToken::new("xapp-test-app");
        let exchange = SlackCodeExchange {
            client_id: "client-id-test".into(),
            client_secret: "client-secret-test".into(),
            code: "code-test".into(),
        };
        let shown = format!("{bot:?} {app:?} {exchange:?}");
        assert!(!shown.contains("xoxb-test-bot"));
        assert!(!shown.contains("xapp-test-app"));
        assert!(!shown.contains("client-secret-test"));
        assert!(!shown.contains("code-test"));
        assert_eq!(bot.reveal(), "xoxb-test-bot");
        assert_eq!(app.reveal(), "xapp-test-app");
    }

    #[test]
    fn api_error_keeps_only_safe_codes() {
        assert_eq!(
            SlackApiError::api("not_in_channel"),
            SlackApiError::Api("not_in_channel".into())
        );
        assert_eq!(
            SlackApiError::api("xoxb-123 leaked"),
            SlackApiError::Api("unknown_error".into())
        );
        assert_eq!(
            SlackApiError::api("").to_string(),
            "Slack returned unknown_error"
        );
    }
}
